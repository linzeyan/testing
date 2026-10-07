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

    /// The fields `path` names, from a root field of `op` down.
    fn resolve(&self, op: Operation, path: &[String]) -> Option<Vec<&Field>> {
        let mut fields = match op {
            Operation::Query => &self.query[..],
            Operation::Mutation => &self.mutation[..],
        };
        let mut out = Vec::new();
        for name in path {
            let f = fields.iter().find(|f| &f.name == name)?;
            out.push(f);
            fields = self.types.get(&f.base).map_or(&[][..], |t| &t.fields);
        }
        (!out.is_empty()).then_some(out)
    }

    /// `path[0]` with the rest of the path inside it, a line each, `indent` levels in.
    /// The last field selects every field of its result (see `selection`). `calls[i]`
    /// is `path[i]`'s argument list.
    fn path_lines(&self, path: &[&Field], calls: &[String], indent: usize) -> Vec<String> {
        let pad = "  ".repeat(indent);
        let (f, call) = (path[0], &calls[0]);
        let inner = match path.len() {
            1 if !self.composite(&f.base) => return vec![format!("{pad}{}{call}", f.name)],
            1 => {
                let mut lines = self.selection(&f.base, indent + 1, &mut Vec::new());
                // A union, or an object with nothing to select bare, still needs one.
                if lines.is_empty() {
                    lines.push(format!("{pad}  __typename"));
                }
                lines
            }
            _ => self.path_lines(&path[1..], &calls[1..], indent + 1),
        };
        let mut lines = vec![format!("{pad}{}{call} {{", f.name)];
        lines.extend(inner);
        lines.push(format!("{pad}}}"));
        lines
    }

    /// An operation down `path` (field names from a root field of `op`), every argument
    /// on the way a variable, the last field selecting every field of its result.
    /// Returns (query text, variables JSON).
    pub fn operation(&self, op: Operation, path: &[String]) -> Option<(String, String)> {
        let fields = self.resolve(op, path)?;
        let mut decls = Vec::new();
        let calls: Vec<_> = (fields.iter()).map(|f| call(f, &[], &mut decls)).collect();
        let mut name: Vec<char> = fields[0].name.chars().collect();
        if let Some(c) = name.first_mut() {
            *c = c.to_ascii_uppercase();
        }
        let name: String = name.into_iter().collect();
        let head = match decls.is_empty() {
            true => String::new(),
            false => format!("({})", declare(&decls)),
        };
        let lines = self.path_lines(&fields, &calls, 1);
        let text = format!(
            "{} {name}{head} {{\n{}\n}}\n",
            op.keyword(),
            lines.join("\n")
        );
        Some((text, with_variables("", &decls)))
    }

    /// `query` with `path` (as in `operation`) added to its first operation: what of the
    /// path isn't there yet goes inside the part that is, new arguments declared as
    /// variables (`variables` gets them too). The rest of the text is left as written.
    /// A query that has no such operation, or doesn't parse, becomes `operation`'s.
    pub fn add(
        &self,
        op: Operation,
        path: &[String],
        query: &str,
        variables: &str,
    ) -> Option<(String, String)> {
        let fields = self.resolve(op, path)?;
        let doc = Parser::new(query).operation();
        let Some(doc) = doc.filter(|d| {
            d.keyword == op.keyword() || (d.keyword.is_empty() && op == Operation::Query)
        }) else {
            return self.operation(op, path);
        };
        let (mut sel, mut k, mut bare) = (&doc.sel, 0, None);
        while let Some(f) = fields.get(k) {
            let Some(found) = sel.fields.iter().find(|x| x.name == f.name) else {
                break;
            };
            k += 1;
            match &found.sel {
                Some(s) => sel = s,
                // Selected bare, though more of the path goes inside it.
                None => {
                    bare = Some(found.end);
                    break;
                }
            }
        }
        if k == fields.len() {
            return Some((query.to_owned(), variables.to_owned()));
        }
        let mut decls = Vec::new();
        let calls: Vec<_> = (fields[k..].iter())
            .map(|f| call(f, &doc.declared, &mut decls))
            .collect();
        let lines = self.path_lines(&fields[k..], &calls, 0);
        let inline = lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
        let close = sel.close;
        let line_start = query[..close].rfind('\n').map_or(0, |n| n + 1);
        let body = match bare {
            Some(end) => (end, format!(" {{ {inline} }}")),
            // The `}` on a line of its own: the new lines go above it, a level further in.
            None if query[line_start..close].trim().is_empty() => {
                let pad = format!("{}  ", &query[line_start..close]);
                let text = lines.iter().map(|l| format!("{pad}{l}\n")).collect();
                (line_start, text)
            }
            None => {
                let lead = match query[..close].ends_with(char::is_whitespace) {
                    true => "",
                    false => " ",
                };
                (close, format!("{lead}{inline} "))
            }
        };
        let mut text = query.to_owned();
        text.insert_str(body.0, &body.1);
        // Before the body, so its offset still holds.
        if !decls.is_empty() {
            let list = declare(&decls);
            let (at, head) = match (doc.keyword.is_empty(), doc.vars) {
                // `{ … }` can't declare variables; `query (…) { … }` can.
                (true, _) => (doc.open, format!("query ({list}) ")),
                (false, Some(close)) => match doc.declared.is_empty() {
                    true => (close, list),
                    false => (close, format!(", {list}")),
                },
                (false, None) => (doc.head, format!("({list})")),
            };
            text.insert_str(at, &head);
        }
        Some((text, with_variables(variables, &decls)))
    }
}

impl Operation {
    fn keyword(self) -> &'static str {
        match self {
            Operation::Query => "query",
            Operation::Mutation => "mutation",
        }
    }
}

/// `f`'s arguments as a call, `(id: $id)`, each a variable added to `decls` (name, type);
/// one taken already (in `declared` or `decls`) gets a number, `$id2`.
fn call(f: &Field, declared: &[String], decls: &mut Vec<(String, String)>) -> String {
    if f.args.is_empty() {
        return String::new();
    }
    let mut args = Vec::new();
    for (arg, ty) in &f.args {
        let taken = |n: &str| {
            declared
                .iter()
                .chain(decls.iter().map(|d| &d.0))
                .any(|d| d == n)
        };
        let mut name = arg.clone();
        let mut n = 1;
        while taken(&name) {
            n += 1;
            name = format!("{arg}{n}");
        }
        args.push(format!("{arg}: ${name}"));
        decls.push((name, ty.clone()));
    }
    format!("({})", args.join(", "))
}

fn declare(decls: &[(String, String)]) -> String {
    let list: Vec<_> = decls.iter().map(|(n, t)| format!("${n}: {t}")).collect();
    list.join(", ")
}

/// `variables` (a JSON object) with a null for each new variable. Text that isn't a JSON
/// object is the user's to fix; it's left alone.
fn with_variables(variables: &str, decls: &[(String, String)]) -> String {
    if decls.is_empty() {
        return variables.to_owned();
    }
    let mut map = match variables.trim() {
        "" => serde_json::Map::new(),
        v => match serde_json::from_str(v) {
            Ok(map) => map,
            Err(_) => return variables.to_owned(),
        },
    };
    for (name, _) in decls {
        map.entry(name.as_str()).or_insert(Value::Null);
    }
    serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_default()
}

/// A selection set in a query's text: where its `}` is, and its fields.
struct Selection {
    close: usize,
    fields: Vec<Selected>,
}

struct Selected {
    /// The field's name, not its alias.
    name: String,
    /// Where it ends before its selection set: past arguments and directives.
    end: usize,
    sel: Option<Selection>,
}

/// A document's operation, with the places `add` writes to.
struct OperationText {
    /// `query`, `mutation`, `subscription`, or empty for the `{ … }` shorthand.
    keyword: String,
    /// Past the keyword and name: where a variable list would go.
    head: usize,
    /// The `)` closing its variable list, if it has one.
    vars: Option<usize>,
    declared: Vec<String>,
    /// Its `{`.
    open: usize,
    sel: Selection,
}

/// Just enough of a GraphQL parser to find where a path's fields are in a query: tokens
/// as byte ranges, strings and comments skipped whole so a brace in them is no brace.
struct Parser<'a> {
    src: &'a str,
    tokens: Vec<(usize, usize)>,
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        let b = src.as_bytes();
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let start = i;
            match b[i] {
                // Commas are whitespace in GraphQL.
                b' ' | b'\t' | b'\r' | b'\n' | b',' => {
                    i += 1;
                    continue;
                }
                b'#' => {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'"' if b[i..].starts_with(b"\"\"\"") => {
                    i += 3;
                    while i < b.len() && !b[i..].starts_with(b"\"\"\"") {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                    i = (i + 3).min(b.len());
                }
                b'"' => {
                    i += 1;
                    while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                    i = (i + 1).min(b.len());
                }
                b'.' if b[i..].starts_with(b"...") => i += 3,
                c if c == b'_' || c.is_ascii_alphabetic() => {
                    while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                        i += 1;
                    }
                }
                c if c == b'-' || c.is_ascii_digit() => {
                    while i < b.len() && (b"-+.".contains(&b[i]) || b[i].is_ascii_alphanumeric()) {
                        i += 1;
                    }
                }
                _ => i += src[i..].chars().next().map_or(1, char::len_utf8),
            }
            tokens.push((start, i));
        }
        Parser { src, tokens, i: 0 }
    }

    fn peek(&self) -> &'a str {
        let src = self.src;
        self.tokens.get(self.i).map_or("", |&(s, e)| &src[s..e])
    }

    fn next(&mut self) -> &'a str {
        let t = self.peek();
        self.i += 1;
        t
    }

    /// Where the token at `i` starts, or the text's end.
    fn at(&self, i: usize) -> usize {
        self.tokens.get(i).map_or(self.src.len(), |t| t.0)
    }

    fn name(t: &str) -> bool {
        t.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
    }

    /// Past a parenthesized list, if one is next.
    fn parens(&mut self) -> Option<()> {
        if self.peek() != "(" {
            return Some(());
        }
        let mut depth = 0;
        loop {
            match self.next() {
                "(" => depth += 1,
                ")" => depth -= 1,
                "" => return None,
                _ => {}
            }
            if depth == 0 {
                return Some(());
            }
        }
    }

    fn directives(&mut self) -> Option<()> {
        while self.peek() == "@" {
            self.i += 2;
            self.parens()?;
        }
        Some(())
    }

    fn selection(&mut self) -> Option<Selection> {
        if self.next() != "{" {
            return None;
        }
        let mut fields = Vec::new();
        loop {
            match self.next() {
                "}" => {
                    let close = self.at(self.i - 1);
                    return Some(Selection { close, fields });
                }
                // A fragment: not looked into, the path goes in beside it.
                "..." => {
                    match self.peek() {
                        "on" => self.i += 2,
                        t if Self::name(t) => self.i += 1,
                        _ => {}
                    }
                    self.directives()?;
                    if self.peek() == "{" {
                        self.selection()?;
                    }
                }
                t if Self::name(t) => {
                    let mut name = t;
                    if self.peek() == ":" {
                        self.i += 1;
                        name = self.next();
                    }
                    self.parens()?;
                    self.directives()?;
                    let end = self.tokens[self.i - 1].1;
                    let sel = match self.peek() {
                        "{" => Some(self.selection()?),
                        _ => None,
                    };
                    fields.push(Selected {
                        name: name.to_owned(),
                        end,
                        sel,
                    });
                }
                _ => return None,
            }
        }
    }

    /// The document's first operation, past any fragments before it.
    fn operation(mut self) -> Option<OperationText> {
        loop {
            match self.peek() {
                "fragment" => {
                    // fragment Name on Type
                    self.i += 4;
                    self.directives()?;
                    self.selection()?;
                }
                "{" => {
                    let open = self.at(self.i);
                    let sel = self.selection()?;
                    let keyword = String::new();
                    let (head, vars, declared) = (open, None, Vec::new());
                    return Some(OperationText {
                        keyword,
                        head,
                        vars,
                        declared,
                        open,
                        sel,
                    });
                }
                k @ ("query" | "mutation" | "subscription") => {
                    self.i += 1;
                    if Self::name(self.peek()) {
                        self.i += 1;
                    }
                    let head = self.tokens[self.i - 1].1;
                    let (mut vars, mut declared) = (None, Vec::new());
                    if self.peek() == "(" {
                        let from = self.i;
                        self.parens()?;
                        vars = Some(self.at(self.i - 1));
                        for i in from..self.i {
                            let (s, e) = self.tokens[i];
                            if &self.src[s..e] == "$" {
                                let (s, e) = self.tokens[i + 1];
                                declared.push(self.src[s..e].to_owned());
                            }
                        }
                    }
                    self.directives()?;
                    let open = self.at(self.i);
                    let sel = self.selection()?;
                    let keyword = k.to_owned();
                    return Some(OperationText {
                        keyword,
                        head,
                        vars,
                        declared,
                        open,
                        sel,
                    });
                }
                _ => return None,
            }
        }
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

    fn path(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn clicking_a_field_writes_a_runnable_operation_with_variables() {
        let schema = parse(&sample()).unwrap();
        let (text, vars) = schema
            .operation(Operation::Query, &path(&["user"]))
            .unwrap();
        assert_eq!(
            text,
            "query User($id: ID!) {\n  user(id: $id) {\n    id\n    name\n    role\n  }\n}\n"
        );
        assert_eq!(vars, "{\n  \"id\": null\n}");
        // Scalar results take no selection set; nested object fields are left out, enums
        // (leaves too) are in.
        let (text, vars) = schema
            .operation(Operation::Query, &path(&["version"]))
            .unwrap();
        assert_eq!(text, "query Version {\n  version\n}\n");
        assert_eq!(vars, "");
    }

    /// A field clicked at any depth brings only its path along, into the query being
    /// built: what's written stays as written (aliases, one-line style, strings with
    /// braces, comments), and every argument on the new part is declared and in the
    /// variables, so the query still runs.
    #[test]
    fn a_nested_click_adds_its_path_to_the_query() {
        let schema = parse(&sample()).unwrap();
        let add =
            |p: &[&str], q: &str, v: &str| schema.add(Operation::Query, &path(p), q, v).unwrap();

        let (q, v) = add(&["user", "role"], "", "");
        assert_eq!(
            q,
            "query User($id: ID!) {\n  user(id: $id) {\n    role\n  }\n}\n"
        );
        assert_eq!(v, "{\n  \"id\": null\n}");

        let (q, v2) = add(&["user", "friends", "name"], &q, &v);
        let nested = "query User($id: ID!) {\n  user(id: $id) {\n    role\n    friends {\n      name\n    }\n  }\n}\n";
        assert_eq!(q, nested);
        assert_eq!(v2, v, "nothing new to declare");
        assert_eq!(
            add(&["user", "friends", "name"], &q, &v),
            (q.clone(), v.clone()),
            "there already"
        );

        let (q, v) = add(&["users", "id"], &q, &v);
        assert_eq!(
            q,
            "query User($id: ID!, $first: Int) {\n  user(id: $id) {\n    role\n    friends {\n      name\n    }\n  }\n  users(first: $first) {\n    id\n  }\n}\n"
        );
        assert_eq!(v, "{\n  \"id\": null,\n  \"first\": null\n}");

        let one_line = "{ me: user(id: \"}\") { id } } # {";
        assert_eq!(
            add(&["user", "name"], one_line, "").0,
            "{ me: user(id: \"}\") { id name } } # {"
        );
        // The shorthand can't declare a variable; `query (…)` can.
        assert_eq!(
            add(&["users", "id"], one_line, ""),
            (
                "query ($first: Int) { me: user(id: \"}\") { id } users(first: $first) { id } } # {".into(),
                "{\n  \"first\": null\n}".into()
            )
        );
        assert_eq!(
            add(&["user", "name"], "query { user }", "").0,
            "query { user { name } }"
        );

        // A name already declared gets a number; values already given stay.
        let (q, v) = add(
            &["user", "id"],
            "query Q($id: String) {\n  version\n}\n",
            "{\"id\": \"x\"}",
        );
        assert_eq!(
            q,
            "query Q($id: String, $id2: ID!) {\n  version\n  user(id: $id2) {\n    id\n  }\n}\n"
        );
        assert_eq!(v, "{\n  \"id\": \"x\",\n  \"id2\": null\n}");

        // Nothing to add to (another kind of operation, or text that doesn't parse): the
        // path starts a new one.
        let fresh = schema.operation(Operation::Query, &path(&["user", "role"]));
        assert_eq!(Some(add(&["user", "role"], "mutation { x }", "")), fresh);
        assert_eq!(Some(add(&["user", "role"], "query { user(", "")), fresh);
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
        let (text, _) = schema.operation(Operation::Query, &path(&["me"])).unwrap();
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
