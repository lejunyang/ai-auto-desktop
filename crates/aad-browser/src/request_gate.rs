//! Operator-owned HTTP policy service. Credentials never enter a browser request.
use reqwest::blocking::{Client, RequestBuilder};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{io::Read, time::Duration};
use url::{Host, Url};

#[derive(Clone)]
pub struct Gate {
    client: Client,
    endpoint: Url,
    configured_url: String,
    token: String,
}

#[derive(Clone, Deserialize)]
pub struct Scope {
    pub revision: String,
    pub watch: Vec<Watch>,
}

#[derive(Clone, Deserialize)]
pub struct Watch {
    pub host: Option<String>,
    pub methods: Option<Vec<String>>,
}

impl Scope {
    pub fn matches(&self, url: &Url, method: &str) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host.trim_end_matches('.');
        self.watch.iter().any(|rule| {
            rule.methods
                .as_ref()
                .is_none_or(|ms| ms.iter().any(|m| m == method))
                && rule.host.as_ref().is_none_or(|pattern| {
                    let pattern = pattern.trim_end_matches('.').to_ascii_lowercase();
                    match pattern.strip_prefix("*.") {
                        Some(suffix) => host
                            .strip_suffix(suffix)
                            .is_some_and(|prefix| prefix.ends_with('.')),
                        None => host == pattern,
                    }
                })
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "decision", rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny {
        message: String,
    },
    Pending {
        message: String,
        #[serde(rename = "askId")]
        ask_id: String,
    },
}

impl Gate {
    pub fn from_environment() -> Result<Option<Self>, &'static str> {
        let read = |key| std::env::var(key).ok();
        let pair = match (read("AAD_REQUEST_GATE_URL"), read("AAD_REQUEST_GATE_TOKEN")) {
            (None, None) => read("OMEM_URL")
                .zip(read("OMEM_TOKEN"))
                .map(|(base, token)| {
                    (
                        format!("{}/api/operation-gate/http", base.trim_end_matches('/')),
                        token,
                    )
                }),
            (Some(endpoint), Some(token)) => Some((endpoint, token)),
            _ => return Err("incomplete request gate configuration"),
        };
        pair.map(|(endpoint, token)| Self::new(&endpoint, token))
            .transpose()
    }

    pub(super) fn credentials(&self) -> (&str, &str) {
        (&self.configured_url, &self.token)
    }
    pub fn new(endpoint: &str, token: String) -> Result<Self, &'static str> {
        let invalid = "invalid loopback request gate configuration";
        let configured_url = endpoint.to_owned();
        let mut endpoint = Url::parse(endpoint).map_err(|_| invalid)?;
        if endpoint.scheme() != "http"
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
            || token.is_empty()
            || !token.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(invalid);
        }
        match endpoint.host() {
            Some(Host::Ipv4(ip)) if ip.is_loopback() => {}
            Some(Host::Ipv6(ip)) if ip.is_loopback() => {}
            Some(Host::Domain("localhost")) => {
                endpoint.set_host(Some("127.0.0.1")).map_err(|_| invalid)?
            }
            _ => return Err(invalid),
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .build()
            .map_err(|_| invalid)?;
        Ok(Self {
            client,
            configured_url,
            endpoint,
            token,
        })
    }

    fn call<T: serde::de::DeserializeOwned>(&self, req: RequestBuilder) -> Result<T, &'static str> {
        let response = req.bearer_auth(&self.token).send().map_err(|e| {
            if e.is_timeout() {
                "request gate timed out"
            } else {
                "request gate unavailable"
            }
        })?;
        if response.status().as_u16() == 401 {
            return Err("request gate token expired");
        }
        if !response.status().is_success() {
            return Err("request gate unavailable");
        }
        let mut bytes = Vec::new();
        response
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(|_| "request gate response unavailable")?;
        if bytes.len() > 1_048_576 {
            return Err("oversized request gate response");
        }
        serde_json::from_slice(&bytes).map_err(|_| "invalid request gate response")
    }
    pub fn scope(&self) -> Result<Scope, &'static str> {
        self.call(
            self.client
                .get(self.endpoint.clone())
                .timeout(Duration::from_secs(3)),
        )
    }
    pub fn decide(&self, payload: &Value) -> Result<Decision, &'static str> {
        self.call(
            self.client
                .post(self.endpoint.clone())
                .timeout(Duration::from_secs(125))
                .json(payload),
        )
    }
    pub fn redact(&self, text: &str) -> String {
        text.replace(&self.configured_url, "[redacted]")
            .replace(self.endpoint.as_str(), "[redacted]")
            .replace(&self.endpoint.origin().ascii_serialization(), "[redacted]")
            .replace(&self.token, "[redacted]")
    }
}

pub fn payload(request: &Value, page: Value, session: &str) -> Value {
    let mut value =
        json!({"method":request["method"],"url":request["url"],"page":page,"session":session});
    if let Some(ty) = request["headers"]
        .as_object()
        .and_then(|h| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        })
        .and_then(|(_, v)| v.as_str())
    {
        value["contentType"] = json!(ty);
        let mime = ty
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if mime.starts_with("text/")
            || mime == "application/json"
            || mime.ends_with("+json")
            || mime == "application/x-www-form-urlencoded"
        {
            if let Some(text) = request["postData"].as_str() {
                let limit = crate::recording::network::truncate(text, 65536).0.len();
                value["body"] = json!(&text[..limit]);
                if limit < text.len() {
                    value["bodyTruncated"] = json!(true)
                }
            }
        }
    }
    value
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_only_and_suffix_boundary() {
        assert!(Gate::new("http://example.com/policy", "token".into()).is_err());
        let s: Scope = serde_json::from_value(
            json!({"revision":"1","watch":[{"host":"*.example.com","methods":["POST"]}]}),
        )
        .unwrap();
        for (host, method, expected) in [
            ("api.example.com", "POST", true),
            ("badexample.com", "POST", false),
            ("example.com", "POST", false),
            ("api.example.com", "GET", false),
        ] {
            assert_eq!(
                s.matches(&Url::parse(&format!("https://{host}/")).unwrap(), method),
                expected
            );
        }
    }
    #[test]
    fn limits_body_on_character_boundary() {
        let mut req = json!({"headers":{"Content-Type":"application/json; charset=utf-8"},"postData":format!("{}é","a".repeat(65535))});
        let v = payload(&req, Value::Null, "test");
        assert_eq!(v["body"].as_str().unwrap().len(), 65535);
        assert_eq!(v["bodyTruncated"], true);
        req["headers"]["Content-Type"] = json!("application/octet-stream");
        assert!(payload(&req, Value::Null, "test").get("body").is_none());
    }
    #[test]
    fn fake_gate_checks_authentication_and_decision() {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let gate = Gate::new(
            &format!("http://{}/policy", listener.local_addr().unwrap()),
            "sample-token".into(),
        )
        .unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let replies = [
                    ("GET","200 OK",r#"{"revision":"a","watch":[]}"#),
                    ("POST","200 OK",r#"{"decision":"pending","message":"review","askId":"one"}"#),
                    ("POST","401 Unauthorized","")
                ];
                for (verb,status,body) in replies {
                    let (mut stream,_) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    let mut input=BufReader::new(&stream);
                    let mut headers=String::new();
                    loop {
                        let mut line=String::new();
                        assert_ne!(input.read_line(&mut line).unwrap(),0);
                        if line=="\r\n"{break}
                        headers.push_str(&line);
                    }
                    assert!(headers.starts_with(&format!("{verb} /policy ")));
                    assert!(headers.to_lowercase().contains("authorization: bearer sample-token"));
                    if verb=="POST"{
                        let mut buf=[0;2];
                        input.read_exact(&mut buf).unwrap();
                        assert_eq!(&buf,b"{}");
                    }
                    write!(stream,"HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
                }
            });
            assert!(gate.scope().unwrap().watch.is_empty());
            assert!(matches!(
                gate.decide(&json!({})),
                Ok(Decision::Pending { .. })
            ));
            assert!(matches!(
                gate.decide(&json!({})),
                Err("request gate token expired")
            ));
        });
    }
}
