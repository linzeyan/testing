//! The one Import box: what was pasted, dropped or pointed to is recognised by its
//! shape, so nobody has to know which menu item their file belongs to.

use serde_json::Value;

pub use crate::postman::Import;

/// JSON, else YAML (OpenAPI specs mostly are).
pub fn value(text: &str) -> Result<Value, String> {
    match serde_json::from_str(text) {
        Ok(v) => Ok(v),
        Err(json) if text.trim_start().starts_with(['{', '[']) => Err(format!("not JSON: {json}")),
        Err(_) => serde_yaml_ng::from_str(text).map_err(|e| format!("not JSON or YAML: {e}")),
    }
}

pub fn parse(text: &str) -> Result<Import, String> {
    let v = value(text)?;
    if crate::openapi::is_spec(&v) {
        return Ok(crate::openapi::import(&v));
    }
    if crate::insomnia::is_insomnia(&v) {
        return Ok(crate::insomnia::import(&v));
    }
    if crate::har::is_har(&v) {
        return Ok(crate::har::import(&v));
    }
    crate::postman::from_value(&v).map_err(|_| {
        "not something apitool imports: a Postman collection or environment, an \
         Insomnia export, an OpenAPI/Swagger spec or a HAR file"
            .to_owned()
    })
}
