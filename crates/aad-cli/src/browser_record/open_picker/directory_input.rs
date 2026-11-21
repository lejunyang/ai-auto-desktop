use super::{refused, Client, PageArgs};
use serde_json::{json, Value};
use std::time::Instant;

type Result<T> = std::result::Result<T, (Value, u8)>;

/// Retain the exact HTML input across snapshot creation and native selection.
/// A page can replace an input with one that has identical HTML attributes.
pub(super) struct Input {
    client: Client,
    object: String,
}

impl Input {
    pub(super) fn observe(args: &PageArgs, meta: &Value, deadline: Instant) -> Result<Self> {
        let pages = Client::list_targets_until(args.cdp_port, deadline).map_err(refused)?;
        let page = pages
            .iter()
            .find(|p| p.id == args.page)
            .ok_or_else(|| refused("folder input page closed"))?;
        let mut client = Client::attach_bare_until(page, deadline).map_err(refused)?;
        client.set_deadline(deadline);
        let world = client
            .command(
                "Page.createIsolatedWorld",
                json!({"frameId":meta["frame_id"]}),
            )
            .map_err(refused)?;
        let result = client.command("Runtime.evaluate", json!({
            "contextId": world["executionContextId"], "returnByValue": false,
            "expression": "(()=>{const a=document.querySelectorAll('input[type=file][webkitdirectory]:open'); if(a.length!==1)return null; return {el:a[0],previous:a[0].files}})()"
        })).map_err(refused)?;
        let object = result["result"]["objectId"]
            .as_str()
            .filter(|_| result["exceptionDetails"].is_null())
            .ok_or_else(|| refused("expected one open HTML directory input in the main document"))?
            .to_owned();
        let mut input = Self { client, object };
        input.revalidate()?;
        Ok(input)
    }

    fn call(&mut self, function: &str, args: Value) -> Result<Value> {
        let result = self
            .client
            .command(
                "Runtime.callFunctionOn",
                json!({
                    "objectId":self.object, "functionDeclaration":function,
                    "arguments":args, "returnByValue":true
                }),
            )
            .map_err(refused)?;
        if !result["exceptionDetails"].is_null() {
            return Err(refused("could not inspect the bound folder input"));
        }
        Ok(result["result"]["value"].clone())
    }

    pub(super) fn revalidate(&mut self) -> Result<()> {
        if self.call("function(){return this.el.isConnected && this.el.ownerDocument===document && this.el.matches('input[type=file][webkitdirectory]:open:enabled') && this.el.files===this.previous}", json!([]))? != true {
            return Err(refused("the observed folder input changed"));
        }
        Ok(())
    }

    // Chromium may show its own upload confirmation after the system chooser.
    // Inspect the DOM once without confirming on the user's behalf.
    pub(super) fn file_list(&mut self, manifest: &Value) -> String {
        self.call(r#"function(expected){
          if(!this.el.isConnected || this.el.ownerDocument!==document) return 'unavailable';
          const files=this.el.files;
          if(files===this.previous) return 'pending';
          const names=new Set();
          for(const file of files) {
            const path=file.webkitRelativePath;
            if(!Object.hasOwn(expected,path) || names.has(path) || expected[path]!==file.size) return 'mismatch';
            names.add(path);
          }
          return names.size===Object.keys(expected).length?'verified':'mismatch';
        }"#, json!([{"value":manifest}])).ok()
            .and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "unavailable".into())
    }
}
