//! Unary gRPC over plain reqwest: the `.proto` is compiled at runtime (protox) and messages
//! are converted to/from JSON with prost-reflect, so no codegen and no protoc install.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use http_body_util::BodyExt;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor, SerializeOptions};

use crate::http::{self, Response, error_chain, header_list};
use crate::model::{Body, Request};

/// Relative proto paths resolve against the working directory, which `main` sets to the
/// workspace root.
fn pool(proto: &str) -> Result<DescriptorPool, String> {
    // ponytail: single-entry cache; enough for one request or a runner/load test hammering
    // the same RPC. Key includes mtime so edits to the .proto are picked up.
    static CACHE: Mutex<Option<(PathBuf, SystemTime, DescriptorPool)>> = Mutex::new(None);
    let proto = proto.trim();
    if proto.is_empty() {
        return Err("No .proto file set".into());
    }
    let path = std::path::absolute(proto).map_err(|e| format!("{proto}: {e}"))?;
    let modified = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("{proto}: {e}"))?;
    let mut cache = CACHE.lock().unwrap();
    if let Some((p, m, pool)) = &*cache
        && *p == path
        && *m == modified
    {
        return Ok(pool.clone());
    }
    // The file's own directory and the workspace root cover both `import "b.proto"` and
    // `import "protos/b.proto"` styles.
    let dir = path.parent().unwrap_or(Path::new("."));
    let pool = protox::Compiler::new([dir, Path::new(".")])
        .and_then(|mut c| {
            c.include_imports(true).open_file(&path)?;
            Ok(c.descriptor_pool())
        })
        .map_err(|e| format!("{proto}: {e}"))?;
    *cache = Some((path, modified, pool.clone()));
    Ok(pool)
}

/// Every `package.Service/Method` in the file, for the method picker.
pub fn methods(proto: &str) -> Result<Vec<String>, String> {
    let pool = pool(proto)?;
    Ok(pool
        .services()
        .flat_map(|s| {
            let service = s.full_name().to_owned();
            s.methods()
                .map(move |m| format!("{service}/{}", m.name()))
                .collect::<Vec<_>>()
        })
        .collect())
}

fn method(proto: &str, rpc: &str) -> Result<MethodDescriptor, String> {
    let pool = pool(proto)?;
    let (service, name) = rpc
        .trim()
        .rsplit_once('/')
        .ok_or_else(|| format!("Method \"{rpc}\" should look like package.Service/Method"))?;
    let service = pool
        .get_service_by_name(service)
        .ok_or_else(|| format!("Service {service} not found in {proto}"))?;
    let method = service
        .methods()
        .find(|m| m.name() == name)
        .ok_or_else(|| format!("Method {name} not found in {}", service.full_name()))?;
    if method.is_client_streaming() || method.is_server_streaming() {
        return Err(format!(
            "{rpc} is a streaming RPC; only unary calls are supported"
        ));
    }
    Ok(method)
}

/// JSON skeleton of the request message with every field present, so users see the shape.
pub fn template(proto: &str, rpc: &str) -> Result<String, String> {
    let msg = DynamicMessage::new(method(proto, rpc)?.input());
    let mut out = serde_json::Serializer::pretty(Vec::new());
    msg.serialize_with_options(
        &mut out,
        &SerializeOptions::new().skip_default_fields(false),
    )
    .map_err(|e| e.to_string())?;
    Ok(String::from_utf8(out.into_inner()).unwrap_or_default())
}

pub async fn call(client: reqwest::Client, req: Request) -> Result<Response, String> {
    let method = method(&req.proto, &req.rpc)?;
    let json = match &req.body {
        Body::Json { text } | Body::Text { text } if !text.trim().is_empty() => text.as_str(),
        _ => "{}",
    };
    let mut de = serde_json::Deserializer::from_str(json);
    let msg = DynamicMessage::deserialize(method.input(), &mut de)
        .map_err(|e| format!("Request message: {e}"))?;
    let payload = msg.encode_to_vec();
    // Length-prefixed message: 1 byte "compressed" flag + u32 big-endian length.
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);

    let url = format!(
        "{}/{}",
        req.url.trim().trim_end_matches('/'),
        req.rpc.trim()
    );
    let wire = Request {
        url,
        body: Body::None,
        ..req
    };
    let started = Instant::now();
    let resp = http::build(&client, wire)?
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(frame)
        .send()
        .await
        .map_err(|e| error_chain(&e))?;
    let status = resp.status();
    let version = format!("{:?}", resp.version());
    let mut headers = header_list(resp.headers());
    let collected = reqwest::Body::from(resp)
        .collect()
        .await
        .map_err(|e| error_chain(&e))?;
    // Trailers-only responses (typical for errors) carry grpc-status in the headers instead.
    if let Some(t) = collected.trailers() {
        headers.extend(header_list(t));
    }
    let elapsed = started.elapsed();
    let bytes = collected.to_bytes();
    let get = |k: &str| {
        headers
            .iter()
            .find(|(h, _)| h == k)
            .map(|(_, v)| v.as_str())
    };
    let code = get("grpc-status").unwrap_or("");
    if !status.is_success() {
        return Err(format!("HTTP {status} (is this a gRPC endpoint?)"));
    }
    if code != "0" {
        let message = get("grpc-message").unwrap_or("");
        let name = code_name(code);
        return Err(format!("gRPC {name} ({code}): {message}"));
    }

    let mut messages = Vec::new();
    let mut rest = &bytes[..];
    while rest.len() >= 5 {
        let len = u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
        if rest[0] != 0 {
            return Err("Compressed gRPC responses are not supported".into());
        }
        let body = rest
            .get(5..5 + len)
            .ok_or("Truncated gRPC response frame")?;
        let msg = DynamicMessage::decode(method.output(), body)
            .map_err(|e| format!("Response message: {e}"))?;
        messages.push(serde_json::to_value(&msg).map_err(|e| e.to_string())?);
        rest = &rest[5 + len..];
    }
    let body = match messages.len() {
        1 => messages.pop().unwrap(),
        _ => serde_json::Value::Array(messages),
    };
    headers.push(("content-type".into(), "application/json".into()));
    Ok(Response {
        status: status.as_u16(),
        reason: "OK".into(),
        version,
        elapsed,
        headers,
        body: body.to_string(),
    })
}

fn code_name(code: &str) -> &'static str {
    const NAMES: [&str; 17] = [
        "OK",
        "CANCELLED",
        "UNKNOWN",
        "INVALID_ARGUMENT",
        "DEADLINE_EXCEEDED",
        "NOT_FOUND",
        "ALREADY_EXISTS",
        "PERMISSION_DENIED",
        "RESOURCE_EXHAUSTED",
        "FAILED_PRECONDITION",
        "ABORTED",
        "OUT_OF_RANGE",
        "UNIMPLEMENTED",
        "INTERNAL",
        "UNAVAILABLE",
        "DATA_LOSS",
        "UNAUTHENTICATED",
    ];
    code.parse::<usize>()
        .ok()
        .and_then(|i| NAMES.get(i).copied())
        .unwrap_or("MISSING_STATUS")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROTO: &str = r#"
        syntax = "proto3";
        package greet.v1;
        service Greeter {
          rpc Hello (HelloRequest) returns (HelloReply);
          rpc Chat (stream HelloRequest) returns (stream HelloReply);
        }
        message HelloRequest { string name = 1; int32 times = 2; }
        message HelloReply { string message = 1; }
    "#;

    fn proto_file() -> String {
        let dir = std::env::temp_dir().join(format!("apitool-grpc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("greet.proto");
        std::fs::write(&path, PROTO).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Minimal h2c gRPC server: replies "hi <name> x<times>", or NOT_FOUND for name "nobody"
    /// as a trailers-only response. Returns the base URL.
    fn server(pool: DescriptorPool) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (sock, _) = listener.accept().await.unwrap();
                let mut conn = h2::server::handshake(sock).await.unwrap();
                while let Some(Ok((req, mut respond))) = conn.accept().await {
                    let pool = pool.clone();
                    tokio::spawn(async move {
                        assert_eq!(req.uri().path(), "/greet.v1.Greeter/Hello");
                        assert_eq!(req.headers()["content-type"], "application/grpc");
                        let mut body = req.into_body();
                        let mut data = Vec::new();
                        while let Some(chunk) = body.data().await {
                            data.extend_from_slice(&chunk.unwrap());
                        }
                        let input = pool.get_message_by_name("greet.v1.HelloRequest").unwrap();
                        let msg = DynamicMessage::decode(input, &data[5..]).unwrap();
                        let name = msg
                            .get_field_by_name("name")
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .to_owned();
                        let times = msg.get_field_by_name("times").unwrap().as_i32().unwrap();
                        let head = |extra: Option<(&str, &str)>| {
                            let mut b = ::http::Response::builder()
                                .header("content-type", "application/grpc");
                            if let Some((k, v)) = extra {
                                b = b.header(k, v).header("grpc-message", "no such user");
                            }
                            b.body(()).unwrap()
                        };
                        if name == "nobody" {
                            respond
                                .send_response(head(Some(("grpc-status", "5"))), true)
                                .unwrap();
                            return;
                        }
                        let mut stream = respond.send_response(head(None), false).unwrap();
                        let output = pool.get_message_by_name("greet.v1.HelloReply").unwrap();
                        let mut reply = DynamicMessage::new(output);
                        reply.set_field_by_name(
                            "message",
                            prost_reflect::Value::String(format!("hi {name} x{times}")),
                        );
                        let payload = reply.encode_to_vec();
                        let mut frame = vec![0];
                        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                        frame.extend_from_slice(&payload);
                        stream.send_data(frame.into(), false).unwrap();
                        let mut trailers = ::http::HeaderMap::new();
                        trailers.insert("grpc-status", "0".parse().unwrap());
                        stream.send_trailers(trailers).unwrap();
                    });
                }
            });
        });
        format!("http://{addr}")
    }

    #[test]
    fn unary_call_round_trips_json_and_surfaces_grpc_errors() {
        let proto = proto_file();
        assert_eq!(
            methods(&proto).unwrap(),
            ["greet.v1.Greeter/Hello", "greet.v1.Greeter/Chat"]
        );
        let tpl: serde_json::Value =
            serde_json::from_str(&template(&proto, "greet.v1.Greeter/Hello").unwrap()).unwrap();
        assert_eq!(tpl, serde_json::json!({"name": "", "times": 0}));
        assert!(
            template(&proto, "greet.v1.Greeter/Chat")
                .unwrap_err()
                .contains("streaming")
        );

        let url = server(pool(&proto).unwrap());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        let client = rt.block_on(crate::net::build_client(net)).unwrap().grpc;
        let req = |name: &str| Request {
            method: "GRPC".into(),
            url: url.clone(),
            proto: proto.clone(),
            rpc: "greet.v1.Greeter/Hello".into(),
            body: Body::Json {
                text: format!(r#"{{"name": "{name}", "times": 3}}"#),
            },
            ..Default::default()
        };
        let resp = rt.block_on(call(client.clone(), req("ada"))).unwrap();
        assert_eq!(resp.body, r#"{"message":"hi ada x3"}"#);
        assert!(
            resp.headers
                .iter()
                .any(|(k, v)| k == "grpc-status" && v == "0")
        );
        let Err(err) = rt.block_on(call(client, req("nobody"))) else {
            panic!("expected NOT_FOUND")
        };
        assert_eq!(err, "gRPC NOT_FOUND (5): no such user");
    }
}
