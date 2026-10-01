//! GraphQL schema introspection and operation generation for the schema explorer.

use std::collections::HashMap;

use serde_json::Value;

/// Only what the explorer shows: root fields, argument types and object fields.
pub const INTROSPECTION: &str = "query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    types {
      kind
      name
      fields(includeDeprecated: false) {
        name
        description
        args { name type { ...TypeRef } }
        type { ...TypeRef }
      }
    }
  }
}
fragment TypeRef on __Type {
  kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name } } } }
}";

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub description: String,
    pub args: Vec<(String, String)>,
    /// As written in SDL, e.g. `[User!]!`.
    pub ty: String,
    /// The named type inside the wrappers, e.g. `User`.
    pub base: String,
}

#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub query: Vec<Field>,
    pub mutation: Vec<Field>,
    /// Fields of every object/interface type, for building selection sets.
    objects: HashMap<String, Vec<Field>>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Operation {
    Query,
    Mutation,
}

/// Parses an introspection response body (`{"data": {"__schema": …}}`).
pub fn parse(body: &str) -> Result<Schema, String> {
    let json: Value =
        serde_json::from_str(body).map_err(|e| format!("response is not JSON: {e}"))?;
    let schema = &json["data"]["__schema"];
    if schema.is_null() {
        let errors = json["errors"].to_string();
        return Err(format!(
            "no schema in the response (is introspection disabled?) {errors}"
        ));
    }
    let mut objects = HashMap::new();
    for t in schema["types"].as_array().into_iter().flatten() {
        let (Some(name), Some(fields)) = (t["name"].as_str(), t["fields"].as_array()) else {
            continue;
        };
        if name.starts_with("__") {
            continue;
        }
        objects.insert(name.to_owned(), fields.iter().map(field).collect());
    }
    let root = |key: &str| {
        schema[key]["name"]
            .as_str()
            .and_then(|n| objects.get(n).cloned())
            .unwrap_or_default()
    };
    Ok(Schema {
        query: root("queryType"),
        mutation: root("mutationType"),
        objects,
    })
}

fn field(v: &Value) -> Field {
    let (ty, base) = type_ref(&v["type"]);
    Field {
        name: v["name"].as_str().unwrap_or_default().to_owned(),
        description: v["description"].as_str().unwrap_or_default().to_owned(),
        args: v["args"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| {
                let name = a["name"].as_str().unwrap_or_default().to_owned();
                (name, type_ref(&a["type"]).0)
            })
            .collect(),
        ty,
        base,
    }
}

/// (SDL notation, named base type) of an introspection `__Type` reference.
fn type_ref(t: &Value) -> (String, String) {
    match t["kind"].as_str() {
        Some("NON_NULL") => {
            let (inner, base) = type_ref(&t["ofType"]);
            (format!("{inner}!"), base)
        }
        Some("LIST") => {
            let (inner, base) = type_ref(&t["ofType"]);
            (format!("[{inner}]"), base)
        }
        _ => {
            let name = t["name"].as_str().unwrap_or("?").to_owned();
            (name.clone(), name)
        }
    }
}

impl Schema {
    /// An operation calling `f` with every argument as a variable, selecting the scalar
    /// fields of its result (one level of nesting is enough to start editing from).
    /// Returns (query text, variables as a JSON object).
    pub fn operation(&self, op: Operation, f: &Field) -> (String, serde_json::Map<String, Value>) {
        let keyword = match op {
            Operation::Query => "query",
            Operation::Mutation => "mutation",
        };
        let mut name: Vec<char> = f.name.chars().collect();
        if let Some(c) = name.first_mut() {
            *c = c.to_ascii_uppercase();
        }
        let name: String = name.into_iter().collect();
        let (decls, call, variables) = if f.args.is_empty() {
            (String::new(), String::new(), serde_json::Map::new())
        } else {
            let decls: Vec<_> = f.args.iter().map(|(n, t)| format!("${n}: {t}")).collect();
            let call: Vec<_> = f.args.iter().map(|(n, _)| format!("{n}: ${n}")).collect();
            let vars = f
                .args
                .iter()
                .map(|(n, _)| (n.clone(), Value::Null))
                .collect();
            (
                format!("({})", decls.join(", ")),
                format!("({})", call.join(", ")),
                vars,
            )
        };
        let selection = match self.objects.get(&f.base) {
            Some(fields) => {
                let scalars: Vec<_> = fields
                    .iter()
                    .filter(|sub| !self.objects.contains_key(&sub.base) && sub.args.is_empty())
                    .map(|sub| format!("    {}", sub.name))
                    .collect();
                // An object with only nested objects still needs a valid selection.
                let lines = if scalars.is_empty() {
                    vec!["    __typename".to_owned()]
                } else {
                    scalars
                };
                format!(" {{\n{}\n  }}", lines.join("\n"))
            }
            None => String::new(),
        };
        let text = format!(
            "{keyword} {name}{decls} {{\n  {}{call}{selection}\n}}\n",
            f.name
        );
        (text, variables)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn named(kind: &str, name: &str) -> Value {
        serde_json::json!({ "kind": kind, "name": name })
    }

    fn wrap(kind: &str, inner: Value) -> Value {
        serde_json::json!({ "kind": kind, "name": null, "ofType": inner })
    }

    pub(crate) fn sample() -> String {
        let id = wrap("NON_NULL", named("SCALAR", "ID"));
        let users = wrap(
            "NON_NULL",
            wrap("LIST", wrap("NON_NULL", named("OBJECT", "User"))),
        );
        serde_json::json!({ "data": { "__schema": {
            "queryType": { "name": "Query" },
            "mutationType": null,
            "types": [
                { "kind": "OBJECT", "name": "Query", "fields": [
                    { "name": "users", "args": [
                        { "name": "first", "type": named("SCALAR", "Int") }
                    ], "type": users },
                    { "name": "user", "args": [ { "name": "id", "type": id } ],
                      "type": named("OBJECT", "User") },
                    { "name": "version", "args": [], "type": named("SCALAR", "String") }
                ]},
                { "kind": "OBJECT", "name": "User", "fields": [
                    { "name": "id", "args": [], "type": id },
                    { "name": "name", "args": [], "type": named("SCALAR", "String") },
                    { "name": "friends", "args": [], "type": users }
                ]},
                { "kind": "SCALAR", "name": "String", "fields": null },
                { "kind": "OBJECT", "name": "__Type", "fields": [] }
            ]
        }}})
        .to_string()
    }

    #[test]
    fn introspection_result_becomes_root_fields_with_sdl_types() {
        let schema = parse(&sample()).unwrap();
        let users = &schema.query[0];
        assert_eq!(
            (users.ty.as_str(), users.base.as_str()),
            ("[User!]!", "User")
        );
        assert_eq!(schema.query[1].args, [("id".to_owned(), "ID!".to_owned())]);
        assert!(schema.mutation.is_empty());
        assert!(
            parse(r#"{"errors":[{"message":"introspection disabled"}]}"#)
                .unwrap_err()
                .contains("introspection disabled")
        );
    }

    #[test]
    fn clicking_a_field_writes_a_runnable_operation_with_variables() {
        let schema = parse(&sample()).unwrap();
        let (text, vars) = schema.operation(Operation::Query, &schema.query[1]);
        assert_eq!(
            text,
            "query User($id: ID!) {\n  user(id: $id) {\n    id\n    name\n  }\n}\n"
        );
        assert_eq!(Value::Object(vars), serde_json::json!({ "id": null }));
        // Scalar results take no selection set; nested object fields are left out.
        let (text, _) = schema.operation(Operation::Query, &schema.query[2]);
        assert_eq!(text, "query Version {\n  version\n}\n");
    }
}
