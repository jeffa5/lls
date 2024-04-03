use lls_lib::wordnet;
use lls_lib::wordnet::Synonyms;
use lls_lib::wordnet::WordNet;
use lsp_server::ErrorCode;
use lsp_server::Message;
use lsp_server::Notification;
use lsp_server::Response;
use lsp_server::ResponseError;
use lsp_server::{Connection, IoThreads};
use lsp_types::notification::LogMessage;
use lsp_types::notification::Notification as _;
use lsp_types::request::Request;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::path::PathBuf;

fn log(c: &Connection, message: impl Serialize) {
    c.sender
        .send(Message::Notification(Notification::new(
            LogMessage::METHOD.to_string(),
            message,
        )))
        .unwrap();
}

fn server_capabilities() -> serde_json::Value {
    let cap = lsp_types::ServerCapabilities {
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        ..Default::default()
    };

    serde_json::to_value(cap).unwrap()
}

fn connect() -> (lsp_types::InitializeParams, Connection, IoThreads) {
    let (c, io) = Connection::stdio();
    let caps = server_capabilities();
    let params = c.initialize(caps).unwrap();
    let params: lsp_types::InitializeParams = serde_json::from_value(params).unwrap();
    log(&c, format!("{:?}", params.initialization_options));
    (params, c, io)
}

struct Server {
    dict: Dict,
    shutdown: bool,
}

impl Server {
    fn new(params: lsp_types::InitializeParams) -> Self {
        let default_wordnet = PathBuf::from("wordnet");
        let wordnet_location = match params.initialization_options {
            None => default_wordnet,
            Some(l) => match serde_json::from_value::<HashMap<String, String>>(l) {
                Ok(v) => match v.get("wordnet") {
                    None => default_wordnet,
                    Some(l) => {
                        if l.starts_with("~/") {
                            dirs::home_dir().unwrap().join(l.trim_start_matches("~/"))
                        } else {
                            PathBuf::from(l)
                        }
                    }
                },
                Err(_) => default_wordnet,
            },
        };
        Self {
            dict: Dict::new(&wordnet_location),
            shutdown: false,
        }
    }

    fn serve(mut self, c: Connection) -> Result<(), String> {
        loop {
            match c.receiver.recv().unwrap() {
                Message::Request(r) => {
                    log(&c, format!("Got request {r:?}"));
                    if self.shutdown {
                        c.sender
                            .send(Message::Response(Response {
                                id: r.id,
                                result: None,
                                error: Some(ResponseError {
                                    code: ErrorCode::InvalidRequest as i32,
                                    message: String::from("received request after shutdown"),
                                    data: None,
                                }),
                            }))
                            .unwrap();
                        continue;
                    }

                    match &r.method[..] {
                        lsp_types::request::HoverRequest::METHOD => {
                            let tdp =
                                serde_json::from_value::<lsp_types::TextDocumentPositionParams>(
                                    r.params,
                                )
                                .unwrap();

                            let response = match get_word(tdp) {
                                Some(w) => {
                                    let text = self.dict.info(&w);
                                    let resp = lsp_types::Hover {
                                        contents: lsp_types::HoverContents::Markup(
                                            lsp_types::MarkupContent {
                                                kind: lsp_types::MarkupKind::Markdown,
                                                value: text,
                                            },
                                        ),
                                        range: None,
                                    };
                                    Message::Response(Response {
                                        id: r.id,
                                        result: Some(serde_json::to_value(resp).unwrap()),
                                        error: None,
                                    })
                                }
                                None => Message::Response(Response {
                                    id: r.id,
                                    result: None,
                                    error: None,
                                }),
                            };

                            c.sender.send(response).unwrap()
                        }
                        lsp_types::request::Shutdown::METHOD => {
                            self.shutdown = true;
                            let none: Option<()> = None;
                            c.sender
                                .send(Message::Response(Response::new_ok(r.id, none)))
                                .unwrap()
                        }
                        _ => log(&c, format!("Unmatched request received: {}", r.method)),
                    }
                }
                Message::Response(r) => log(&c, format!("Unmatched response received: {}", r.id)),
                Message::Notification(n) => match &n.method[..] {
                    "exit" => {
                        if self.shutdown {
                            return Ok(());
                        } else {
                            return Err(String::from(
                                "Received exit notification before shutdown request",
                            ));
                        }
                    }
                    _ => log(&c, format!("Unmatched notification received: {}", n.method)),
                },
            }
        }
    }
}

fn main() {
    let (p, c, io) = connect();
    let server = Server::new(p);
    let s = server.serve(c);
    io.join().unwrap();
    match s {
        Ok(()) => (),
        Err(s) => {
            eprintln!("{}", s);
            std::process::exit(1)
        }
    }
}

struct Dict {
    wordnet: WordNet,
}

struct DictItem {
    definitions: Vec<wordnet::Definition>,
    synonyms: Vec<Synonyms>,
}

impl DictItem {
    fn render(&self, word: &str) -> String {
        let mut blocks = Vec::new();

        let mut defs: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for d in self.definitions.iter() {
            defs.entry(d.pos).or_default().push(d.def.clone());
        }

        for (pos, def) in defs {
            let mut s = String::new();
            s.push_str(&format!("**{word}** _{pos}_\n"));
            s.push_str(
                &def.iter()
                    .enumerate()
                    .map(|(i, x)| format!("{}. {}", i + 1, x))
                    .collect::<Vec<String>>()
                    .join("\n"),
            );
            blocks.push(s);

            if let Some(syns) = self.synonyms.iter().find(|s| s.pos == pos) {
                let syns = syns
                    .synonyms
                    .iter()
                    .map(|x| x.replace('_', " "))
                    .collect::<Vec<String>>()
                    .join(", ");
                if !syns.is_empty() {
                    blocks.push(format!("**Synonyms**: {syns}"));
                }
            }
        }

        blocks.join("\n\n")
    }
}

impl Dict {
    fn new(value: &Path) -> Self {
        Self {
            wordnet: WordNet::new(value.to_path_buf()),
        }
    }

    fn info(&mut self, word: &str) -> String {
        let definitions = self.wordnet.definitions(word);
        let synonyms = self.wordnet.synonyms(word);
        let di = DictItem {
            definitions,
            synonyms,
        };
        di.render(word)
    }
}

fn get_word(tdp: lsp_types::TextDocumentPositionParams) -> Option<String> {
    let file = std::fs::File::open(tdp.text_document.uri.to_file_path().unwrap()).unwrap();
    let reader = std::io::BufReader::new(file);
    let line = match reader.lines().nth(tdp.position.line as usize) {
        None => return None,
        Some(l) => match l {
            Err(_) => return None,
            Ok(l) => l,
        },
    };

    let mut current_word = String::new();
    let mut found = false;
    for (i, c) in line.chars().enumerate() {
        if c.is_alphabetic() {
            current_word.push_str(&c.to_lowercase().collect::<String>())
        } else {
            if found {
                return Some(current_word);
            }
            current_word = String::new()
        }

        if i == tdp.position.character as usize {
            found = true
        }

        if !c.is_alphabetic() && found {
            return Some(current_word);
        }
    }

    // got to end of line
    if found {
        return Some(current_word);
    }

    None
}
