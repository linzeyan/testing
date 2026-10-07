//! GraphQL schema introspection and operation generation for the schema explorer.

use std::collections::BTreeMap;

use serde_json::Value;

/// Only what the explorer shows: root fields, argument types, and every type's fields,
/// input fields and enum values.
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
      inputFields { name description type { ...TypeRef } }
      enumValues(includeDeprecated: false) { name description }
    }
  }
}
fragment TypeRef on __Type {
  kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name } } } }
}";

/// Whether the document's (first) operation is a subscription: it then streams over a
/// WebSocket instead of a POST.
pub fn is_subscription(query: &str) -> bool {
    let code = query
        .lines()
        .map(|l| l.split('#').next().unwrap_or_default());
    let code: String = code.collect::<Vec<_>>().join(" ");
    code.trim_start()
        .strip_prefix("subscription")
        .is_some_and(|rest| {
            rest.chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub description: String,
    pub args: Vec<(String, String)>,
    /// As written in SDL, e.g. `[User!]!`; empty for an enum value.
    pub ty: String,
    /// The named type inside the wrappers, e.g. `User`.
    pub base: String,
}

#[derive(Clone, Debug)]
pub struct Type {
    /// `OBJECT`, `INTERFACE`, `INPUT_OBJECT` or `ENUM`.
    pub kind: String,
    /// Its fields, an input's fields, or an enum's values.
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub query: Vec<Field>,
    pub mutation: Vec<Field>,
    /// Every named type that has fields or values, by name.
    types: BTreeMap<String, Type>,
    /// The query and mutation types: their fields are `query` and `mutation`.
    roots: Vec<String>,
}

/// How many objects deep a clicked field's selection goes. A connection takes three
/// (`edges { node { … } }`); past that, a big schema's query runs to thousands of lines.
const SELECT_DEPTH: usize = 4;

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
    let mut types = BTreeMap::new();
    // Scalars and unions too, with no fields: a selection must know them from objects.
    for t in schema["types"].as_array().into_iter().flatten() {
        let Some(name) = t["name"].as_str().filter(|n| !n.starts_with("__")) else {
            continue;
        };
        let fields = (["fields", "inputFields", "enumValues"].iter())
            .find_map(|key| t[*key].as_array())
            .map_or_else(Vec::new, |f| f.iter().map(field).collect());
        let kind = t["kind"].as_str().unwrap_or_default().to_owned();
        types.insert(name.to_owned(), Type { kind, fields });
    }
    let roots: Vec<String> = (["queryType", "mutationType"].iter())
        .filter_map(|key| schema[*key]["name"].as_str().map(str::to_owned))
        .collect();
    let root = |key: &str| {
        let name = schema[key]["name"].as_str();
        (name.and_then(|n| types.get(n))).map_or_else(Vec::new, |t| t.fields.clone())
    };
    Ok(Schema {
        query: root("queryType"),
        mutation: root("mutationType"),
        types,
        roots,
    })
}

fn field(v: &Value) -> Field {
    // An enum value has no type.
    let (ty, base) = match v["type"].is_null() {
        true => Default::default(),
        false => type_ref(&v["type"]),
    };
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
    /// The fields (or values) of the named type, if it has any.
    pub fn fields_of(&self, name: &str) -> Option<&[Field]> {
        (self.types.get(name))
            .map(|t| &t.fields[..])
            .filter(|f| !f.is_empty())
    }

    /// The types to browse, by name: all with fields or values but the query and mutation
    /// types.
    pub fn listed(&self) -> impl Iterator<Item = (&str, &Type)> {
        let roots = &self.roots;
        (self.types.iter())
            .filter(move |(name, t)| !roots.contains(name) && !t.fields.is_empty())
            .map(|(name, t)| (name.as_str(), t))
    }

    /// Whether a selection goes inside a field of this type.
    fn composite(&self, name: &str) -> bool {
        (self.types.get(name))
            .is_some_and(|t| matches!(t.kind.as_str(), "OBJECT" | "INTERFACE" | "UNION"))
    }

    /// Whether a field of this type is selected bare. One the schema doesn't list counts
    /// as a scalar.
    fn leaf(&self, name: &str) -> bool {
        (self.types.get(name)).is_none_or(|t| matches!(t.kind.as_str(), "SCALAR" | "ENUM"))
    }

    /// The selection inside a field of type `base`, a line each, `indent` levels in: its
    /// leaves, and its object fields with theirs, `SELECT_DEPTH` objects down. Left out:
    /// a type already on the way in (`User.friends` would go round forever), a field with
    /// a required argument (it can't go in bare) and a union's (it needs fragments).
    fn selection(&self, base: &str, indent: usize, path: &mut Vec<String>) -> Vec<String> {
        let Some(t) = self.types.get(base).filter(|_| self.composite(base)) else {
            return Vec::new();
        };
        path.push(base.to_owned());
        let pad = "  ".repeat(indent);
        let mut lines = Vec::new();
        for f in &t.fields {
            if f.args.iter().any(|(_, ty)| ty.ends_with('!')) {
                continue;
            }
            if self.leaf(&f.base) {
                lines.push(format!("{pad}{}", f.name));
            } else if path.len() < SELECT_DEPTH && !path.contains(&f.base) {
                let inner = self.selection(&f.base, indent + 1, path);
                if !inner.is_empty() {
                    lines.push(format!("{pad}{} {{", f.name));
                    lines.extend(inner);
                    lines.push(format!("{pad}}}"));
                }
            }
        }
        path.pop();
        lines
    }

    /// An operation calling `f` with every argument as a variable, selecting every field
    /// of its result, nested objects' too (see `selection`).
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
        let selection = match self.composite(&f.base) {
            true => {
                let mut lines = self.selection(&f.base, 2, &mut Vec::new());
                // A union, or an object with nothing to select bare, still needs one.
                if lines.is_empty() {
                    lines.push("    __typename".to_owned());
                }
                format!(" {{\n{}\n  }}", lines.join("\n"))
            }
            false => String::new(),
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
                    { "name": "role", "args": [], "type": named("ENUM", "Role") },
                    { "name": "friends", "args": [], "type": users }
                ]},
                { "kind": "ENUM", "name": "Role", "fields": null, "enumValues": [
                    { "name": "ADMIN", "description": "Can do anything" },
                    { "name": "GUEST" }
                ]},
                { "kind": "INPUT_OBJECT", "name": "UserInput", "fields": null, "inputFields": [
                    { "name": "name", "type": wrap("NON_NULL", named("SCALAR", "String")) }
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
            "query User($id: ID!) {\n  user(id: $id) {\n    id\n    name\n    role\n  }\n}\n"
        );
        assert_eq!(Value::Object(vars), serde_json::json!({ "id": null }));
        // Scalar results take no selection set; nested object fields are left out, enums
        // (leaves too) are in.
        let (text, _) = schema.operation(Operation::Query, &schema.query[2]);
        assert_eq!(text, "query Version {\n  version\n}\n");
    }

    /// A click fills in every level, not only the first: nested objects come with their
    /// own fields, down to SELECT_DEPTH. What would make the query invalid or endless is
    /// left out: a type already on the way in, a required argument, a union.
    #[test]
    fn a_clicked_field_selects_every_level_down() {
        let f = |name: &str, ty: Value| serde_json::json!({ "name": name, "args": [], "type": ty });
        let obj = |name: &str, fields: Value| serde_json::json!({ "kind": "OBJECT", "name": name, "fields": fields });
        let s = |name: &str| named("SCALAR", name);
        let o = |name: &str| named("OBJECT", name);
        let body = serde_json::json!({ "data": { "__schema": {
            "queryType": { "name": "Query" },
            "types": [
                obj("Query", serde_json::json!([f("me", o("User"))])),
                obj("User", serde_json::json!([
                    f("id", s("ID")),
                    f("address", o("Address")),
                    { "name": "posts", "args": [
                        { "name": "first", "type": wrap("NON_NULL", s("Int")) }
                    ], "type": o("Post") },
                    f("best", o("Post")),
                    f("boss", o("User")),
                    f("hit", named("UNION", "Hit")),
                ])),
                obj("Address", serde_json::json!([f("city", s("String")), f("geo", o("Geo"))])),
                obj("Geo", serde_json::json!([f("lat", s("Float")), f("deep", o("L4"))])),
                obj("L4", serde_json::json!([f("x", s("String")), f("y", o("L5"))])),
                obj("L5", serde_json::json!([f("z", s("String"))])),
                obj("Post", serde_json::json!([f("title", s("String")), f("author", o("User"))])),
                { "kind": "UNION", "name": "Hit", "fields": null },
                { "kind": "SCALAR", "name": "String", "fields": null },
            ]
        }}});
        let schema = parse(&body.to_string()).unwrap();
        let (text, _) = schema.operation(Operation::Query, &schema.query[0]);
        assert_eq!(
            text,
            "query Me {
  me {
    id
    address {
      city
      geo {
        lat
        deep {
          x
        }
      }
    }
    best {
      title
    }
  }
}
"
        );
        assert!(
            schema
                .listed()
                .all(|(name, _)| name != "Hit" && name != "String"),
            "nothing to open in a union or scalar"
        );
    }

    /// Every type's fields can be looked at, not only the root ones: an object's fields,
    /// an input's fields and an enum's values; the root types aren't listed twice.
    #[test]
    fn every_type_shows_its_fields_or_values() {
        let schema = parse(&sample()).unwrap();
        let names = |fields: &[Field]| -> Vec<String> {
            fields
                .iter()
                .map(|f| format!("{}: {}", f.name, f.ty))
                .collect()
        };
        let listed: Vec<_> = schema.listed().map(|(n, t)| (n, t.kind.as_str())).collect();
        assert_eq!(
            listed,
            [
                ("Role", "ENUM"),
                ("User", "OBJECT"),
                ("UserInput", "INPUT_OBJECT")
            ]
        );
        assert_eq!(
            names(schema.fields_of("User").unwrap()),
            ["id: ID!", "name: String", "role: Role", "friends: [User!]!"]
        );
        assert_eq!(
            names(schema.fields_of("UserInput").unwrap()),
            ["name: String!"]
        );
        let role = schema.fields_of("Role").unwrap();
        assert_eq!(names(role), ["ADMIN: ", "GUEST: "]);
        assert_eq!(role[0].description, "Can do anything");
        assert!(
            schema.fields_of("String").is_none(),
            "a scalar has nothing to open"
        );
    }
}
