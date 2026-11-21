//! Read a page in a fresh, locally owned WebKit WebDriver session.
use reqwest::blocking::Client;
use reqwest::{redirect, Method};
use serde_json::{json, Value};
use std::io::Read;
use std::time::{Duration, Instant};
use url::Url;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Driver {
    http: Client,
    base: String,
    deadline: Instant,
}

impl Driver {
    fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("WebDriver deadline elapsed".into());
        }
        let request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .timeout(left);
        let response = match body {
            Some(value) => request.json(&value),
            None => request,
        }
        .send()?;
        let status = response.status();
        let mut bytes = Vec::new();
        response.take(262_145).read_to_end(&mut bytes)?;
        if bytes.len() > 262_144 {
            return Err("WebDriver response exceeds 256 KiB".into());
        }
        let mut envelope: Value = serde_json::from_slice(&bytes)?;
        let value = envelope
            .get_mut("value")
            .ok_or("WebDriver response missing value")?
            .take();
        if !status.is_success() {
            let code = value["error"].as_str().unwrap_or("driver error");
            return Err(format!("WebDriver returned {status}: {code}").into());
        }
        Ok(value)
    }
}

/// Own the session across navigation and observation, including cleanup after a failed read.
pub fn read(port: u16, raw_url: &str, outline: bool) -> Result<Value> {
    let url = Url::parse(raw_url)?;
    if port == 0
        || !matches!(url.scheme(), "https" | "http")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(
            "supply a loopback WebDriver port and an HTTP(S) page without credentials".into(),
        );
    }
    let mut driver = Driver {
        http: Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .build()?,
        base: format!("http://127.0.0.1:{port}"),
        deadline: Instant::now() + Duration::from_secs(45),
    };
    let session = driver.request(
        Method::POST,
        "/session",
        Some(json!({
            "capabilities":{"alwaysMatch":{"unhandledPromptBehavior":"ignore",
                "timeouts":{"pageLoad":20000,"script":5000}},
                "firstMatch":[{"browserName":"MiniBrowser"},{"browserName":"Safari"}]}
        })),
    )?;
    let id = session["sessionId"]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        })
        .ok_or("invalid WebDriver session ID")?;
    driver.base.push_str(&format!("/session/{id}"));
    let result = (|| -> Result<Value> {
        let name = session["capabilities"]["browserName"]
            .as_str()
            .unwrap_or("");
        if !["MiniBrowser", "Safari"]
            .iter()
            .any(|n| n.eq_ignore_ascii_case(name))
        {
            return Err("the driver created a non-WebKit session".into());
        }
        if driver.request(Method::GET, "/url", None)? != "about:blank" {
            return Err("the new WebKit window is not blank".into());
        }
        driver.request(Method::POST, "/url", Some(json!({"url":url.as_str()})))?;
        if driver.request(Method::GET, "/url", None)? != url.as_str() {
            return Err("the WebKit page did not arrive at the requested URL".into());
        }
        let mut read = driver.request(
            Method::POST,
            "/execute/sync",
            Some(json!({
                "script":include_str!("webkit_read.js"), "args":[url.as_str(), outline]
            })),
        )?;
        if !read.is_object() || read["url"] != url.as_str() {
            return Err("WebKit document changed before observation".into());
        }
        read["browser"] = json!(name);
        Ok(read)
    })();
    driver.deadline = Instant::now() + Duration::from_secs(5);
    let closed = driver.request(Method::DELETE, "", None);
    let read = result?;
    closed?;
    Ok(read)
}
