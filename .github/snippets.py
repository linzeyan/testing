"""Runs generated snippets against a local server and compares what arrives with what
the cURL snippet (the reference, checked exactly by unit tests) sends.

    APITOOL_SNIPPETS=snip/in cargo test --lib -- every_target
    SNIP_TOOLS=curl,php,java python3 .github/snippets.py snip

SNIP_TOOLS picks the snippets to run (curl is always run: it is the reference);
SNIP_PWSH is the PowerShell to use (`powershell` for Windows PowerShell 5.1);
SNIP_BASH the bash for the shell snippets (on Windows, not WSL's);
SNIP_KOTLIN_CP is the classpath with the OkHttp and Okio jars.
Exits non-zero if any picked snippet failed, differed, or couldn't run."""
import http.server, os, queue, re, shutil, socket, subprocess, sys, threading
from email.parser import BytesParser
from email.policy import default

SNIP = os.path.abspath(sys.argv[1])
TOOLS = set(os.environ.get("SNIP_TOOLS", "").split(",")) - {""}
PWSH = os.environ.get("SNIP_PWSH", "pwsh")
BASH = os.environ.get("SNIP_BASH", "bash")
KOTLIN_CP = os.environ.get("SNIP_KOTLIN_CP", "")
# What a snippet admits it can't do, in a comment at its top: not a failure.
ADMITTED = {"multipart.wget.sh", "multipart.java.java"}
captured = queue.Queue()


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def any(self):
        if "chunked" in self.headers.get("transfer-encoding", "").lower():
            body = b""
            while True:
                size = int(self.rfile.readline().strip().split(b";")[0], 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    break
                body += self.rfile.read(size)
                self.rfile.readline()
        else:
            body = self.rfile.read(int(self.headers.get("content-length") or 0))
        headers = {k.lower(): v for k, v in self.headers.items()}
        captured.put((self.command, self.path, headers, body))
        self.send_response(200)
        self.send_header("content-type", "text/plain")
        self.send_header("content-length", "2")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(b"ok")

    do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = do_OPTIONS = any

    def log_message(self, *a):
        pass


def norm(case, req):
    method, path, h, body = req
    ct = h.get("content-type", "")
    out = {"method": method, "path": path, "authorization": h.get("authorization"),
           "x-note": h.get("x-note")}
    if case == "settings":
        out["accept"] = h.get("accept")
    if ct.startswith("multipart/form-data"):
        msg = BytesParser(policy=default).parsebytes(b"Content-Type: " + ct.encode() + b"\r\n\r\n" + body)
        out["parts"] = [(p.get_param("name", header="content-disposition"), p.get_filename(),
                         p.get_payload(decode=True)) for p in msg.iter_parts()]
    else:
        out["content-type"] = ct.lower() or None
        out["body"] = body
    return out


def send_raw(path, host, upload):
    """Sends a raw-HTTP snippet the way an editor's .http runner would: the placeholders
    filled in, CRLF line ends in the head (and in a multipart body, which has no length
    of its own to keep), a Content-Length added only where the snippet has none."""
    text = open(path, "rb").read()
    head, _, body = text.partition(b"\n\n")
    head = head.replace(b"<size of upload.txt>", str(len(upload)).encode())
    if b"multipart/form-data" in head:
        body = body.replace(b"\n", b"\r\n")
    body = body.replace(b"<contents of upload.txt>", upload)
    if body and not re.search(rb"(?im)^content-length:", head):
        head += b"\ncontent-length: " + str(len(body)).encode()
    wire = head.replace(b"\n", b"\r\n") + b"\r\n\r\n" + body
    with socket.create_connection(host.split(":")) as s:
        s.sendall(wire)
        reply = s.recv(4096)
    return subprocess.CompletedProcess([], 0 if reply.startswith(b"HTTP/1.1 200") else 1,
                                       reply, b"")


def main():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    host = f"127.0.0.1:{server.server_port}"
    work = f"{SNIP}/work"
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(f"{work}/kt")
    upload = b"hello file\n"
    open(f"{work}/upload.txt", "wb").write(upload)
    files = [n for n in sorted(os.listdir(f"{SNIP}/in"))
             if not TOOLS or n.split(".")[1] in TOOLS | {"curl"}]
    for name in files:
        text = open(f"{SNIP}/in/{name}", encoding="utf-8").read().replace("echo.test:8080", host)
        # A BOM so Windows PowerShell 5.1 reads the file as UTF-8, as it would a paste.
        enc = "utf-8-sig" if name.endswith(".ps1") else "utf-8"
        open(f"{work}/{name}", "w", encoding=enc, newline="\n").write(text)

    log = open(f"{SNIP}/build.log", "w")
    npm = any(n.endswith(".cjs") for n in files) and subprocess.run(
        ["npm", "i", "--silent", "--no-audit", "--no-fund", "axios"],
        cwd=work, stdout=log, stderr=log).returncode == 0
    rs = f"{SNIP}/rs"
    cargo = False
    if any(n.endswith(".rs") for n in files):
        os.makedirs(f"{rs}/src/bin", exist_ok=True)
        open(f"{rs}/Cargo.toml", "w").write(
            '[package]\nname = "snip"\nversion = "0.0.0"\nedition = "2024"\n\n[dependencies]\n'
            'reqwest = { version = "0.13", features = ["multipart", "stream"] }\n'
            'tokio = { version = "1", features = ["full"] }\n')
        for name in files:
            if name.endswith(".rs"):
                shutil.copy(f"{work}/{name}", f"{rs}/src/bin/{name.split('.')[0]}.rs")
        cargo = subprocess.run(["cargo", "build", "--quiet"], cwd=rs,
                               stdout=log, stderr=log).returncode == 0
    jar_sep = ";" if os.name == "nt" else ":"

    def run(name):
        case, tool, ext = name.split(".")
        if ext == "http":
            return send_raw(f"{work}/{name}", host, upload)
        if ext == "kt":
            # One file per compile: every snippet has its own top-level main.
            shutil.copy(f"{work}/{name}", f"{work}/kt/{case}.kt")
            jar = f"{work}/kt/{case}.jar"
            built = subprocess.run(["kotlinc", f"kt/{case}.kt", "-cp", KOTLIN_CP, "-d", jar],
                                   cwd=work, capture_output=True, timeout=600)
            if built.returncode != 0:
                return built
            cmd = ["kotlin", "-cp", jar + jar_sep + KOTLIN_CP, case.capitalize() + "Kt"]
        else:
            cmd = {
                "sh": [BASH, name],
                "ps1": [PWSH, "-NoProfile", "-NonInteractive", "-File", name],
                "php": ["php", name],
                "java": ["java", name],
                "py": [sys.executable, name],
                "mjs": ["node", name],
                "cjs": ["node", name] if npm else None,
                "go": ["go", "run", name],
                "rb": ["ruby", name],
                "swift": ["swift", name],
                "cs": ["dotnet", "run", name],
                "rs": [f"{rs}/target/debug/{case}"] if cargo else None,
            }[ext]
            if cmd is None or shutil.which(cmd[0]) is None:
                return None
        return subprocess.run(cmd, cwd=work, capture_output=True, timeout=300)

    results, reference = {}, {}
    for name in sorted(files, key=lambda n: (n.split(".")[0], n.split(".")[1] != "curl")):
        case, tool, _ = name.split(".")
        if name.endswith(".ps1") and PWSH == "powershell" and \
                open(f"{work}/{name}", encoding="utf-8-sig").read().startswith("# Needs PowerShell 7"):
            results[name] = "skipped (says it needs PowerShell 7)"
            continue
        while not captured.empty():
            captured.get()
        try:
            p = run(name)
        except subprocess.TimeoutExpired:
            results[name] = "FAIL timeout"
            continue
        if p is None:
            results[name] = "FAIL not run (no runtime)"
            continue
        open(f"{SNIP}/out-{name}.log", "wb").write(p.stdout + b"\n--- stderr ---\n" + p.stderr)
        if captured.empty():
            results[name] = f"FAIL exit {p.returncode}, nothing arrived"
            continue
        got = norm(case, captured.get())
        if tool == "curl":
            reference[case] = got
            if p.returncode != 0:
                results[name] = f"FAIL reference exit {p.returncode}"
                continue
            results[name] = f"reference (exit {p.returncode})"
            continue
        ref = reference.get(case)
        if ref is None:
            results[name] = "FAIL the cURL reference sent nothing"
            continue
        diff = [k for k in sorted(set(ref) | set(got)) if ref.get(k) != got.get(k)]
        if case == "digest":  # only curl answers the challenge; the rest leave a comment
            diff = [k for k in diff if k != "authorization"]
        if not diff and p.returncode == 0:
            results[name] = "OK"
        else:
            results[name] = ("admitted " if name in ADMITTED else "") + (
                f"DIFF exit {p.returncode} " + "; ".join(
                    f"{k}: {ref.get(k)!r} != {got.get(k)!r}" for k in diff))
    report = "".join(f"{name:28} {results[name]}\n" for name in sorted(results))
    open(f"{SNIP}/report.txt", "w", encoding="utf-8").write(report)
    print(report)
    if os.environ.get("GITHUB_ACTIONS"):
        # Annotations can be read through the API with a plain token, job logs can't.
        print("::notice title=snippets::" + report.replace("%", "%25").replace("\n", "%0A"))
    bad =[n for n, r in results.items() if r.startswith(("FAIL", "DIFF"))]
    for name in bad:
        out = f"{SNIP}/out-{name}.log"
        if os.path.exists(out):
            print(f"===== {name}\n" + open(out, encoding="utf-8", errors="replace").read()[-3000:])
    sys.exit(1 if bad else 0)


main()
