use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

pub struct FakeClaude {
    _dir: TempDir,
    path: PathBuf,
}

impl FakeClaude {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude");
        write_fake_claude_script(&path);
        Self { _dir: dir, path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn write_fake_claude_script(path: &Path) {
    fs::write(
        path,
        r#"#!/usr/bin/env python3
import json
import os
from pathlib import Path
import select
import sys

argv_path = os.environ.get("FAKE_CLAUDE_ARGV_PATH")
if argv_path:
    Path(argv_path).write_text(json.dumps(sys.argv[1:]), encoding="utf-8")

if "--version" in sys.argv or "-v" in sys.argv:
    print("fake claude 0.0.0")
    sys.exit(0)

if "--help" in sys.argv or "-h" in sys.argv:
    print("Usage: claude [options] [prompt]")
    print("  -p, --print")
    print("  --input-format <format>")
    print("  --output-format <format>")
    print("  --agent <agent>")
    print("  --agents <json>")
    sys.exit(0)

def arg_value(flag, default=None):
    if flag in sys.argv:
        idx = sys.argv.index(flag)
        if idx + 1 < len(sys.argv):
            return sys.argv[idx + 1]
    for arg in sys.argv:
        if arg.startswith(flag + "="):
            return arg.split("=", 1)[1]
    return default

def project_key(cwd):
    out = []
    for ch in str(cwd):
        out.append(ch if ch.isascii() and ch.isalnum() else "-")
    return "".join(out) or "-"

session_id = arg_value("--session-id") or arg_value("--resume") or "00000000-0000-0000-0000-000000000000"
config_dir = Path(os.environ.get("CLAUDE_CONFIG_DIR", str(Path.home() / ".claude")))
transcript = config_dir / "projects" / project_key(Path.cwd()) / f"{session_id}.jsonl"
transcript.parent.mkdir(parents=True, exist_ok=True)

sys.stdout.write("Context permissions /mcp\n")
sys.stdout.flush()

buf = b""
while True:
    chunk = os.read(0, 4096)
    if not chunk:
        break
    buf += chunk
    end = buf.find(b"\x1b[201~")
    if end < 0:
        continue
    start = buf.find(b"\x1b[200~")
    raw_prompt = buf[start + len(b"\x1b[200~"):end] if start >= 0 else buf[:end]
    prompt = raw_prompt.decode("utf-8", errors="replace")
    response = "FAKE_RESPONSE: " + prompt
    if "USE_FAKE_TOOL" in prompt:
        with transcript.open("a", encoding="utf-8") as f:
            f.write(json.dumps({"type":"system","subtype":"init","session_id":session_id}) + "\n")
            f.write(json.dumps({"type":"user","message":{"role":"user","content":prompt}}) + "\n")
            f.write(json.dumps({"type":"assistant","message":{"model":"fake-model","content":[{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"echo fake"}}]}}) + "\n")
        sys.stdout.write("Do you want to allow Bash?\n")
        sys.stdout.write("❯ 1. Yes\n")
        sys.stdout.write("  2. No, and tell Claude what to do differently\n")
        sys.stdout.write("Enter to confirm · Esc to cancel\n")
        sys.stdout.flush()
        ack = os.read(0, 4096)
        deny = b"\x1b[B" in ack or b"2" in ack or ack.startswith(b"\x1b")
        deny_reason = ""
        if deny:
            sys.stdout.write("Tell Claude what to do differently\n")
            sys.stdout.flush()
            ready, _, _ = select.select([0], [], [], 2)
            if ready:
                reason_bytes = os.read(0, 4096)
                start_reason = reason_bytes.find(b"\x1b[200~")
                end_reason = reason_bytes.find(b"\x1b[201~")
                if start_reason >= 0 and end_reason >= 0:
                    deny_reason = reason_bytes[start_reason + len(b"\x1b[200~"):end_reason].decode("utf-8", errors="replace")
                else:
                    deny_reason = reason_bytes.decode("utf-8", errors="replace").strip()
        if deny:
            response = "FAKE_TOOL_DENIED"
            if deny_reason:
                response += ": " + deny_reason
        else:
            response = "FAKE_TOOL_ALLOWED"
        with transcript.open("a", encoding="utf-8") as f:
            f.write(json.dumps({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":response}]}}) + "\n")
            f.write(json.dumps({"type":"assistant","message":{"model":"fake-model","content":[{"type":"text","text":response}]}}) + "\n")
            f.write(json.dumps({"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":False,"num_turns":1,"session_id":session_id,"result":response,"usage":{"input_tokens":1,"output_tokens":1}}) + "\n")
        sys.stdout.write("Context permissions /mcp\n")
        sys.stdout.flush()
        after = end + len(b"\x1b[201~")
        while after < len(buf) and buf[after:after + 1] in (b"\r", b"\n"):
            after += 1
        buf = buf[after:]
        continue
    with transcript.open("a", encoding="utf-8") as f:
        f.write(json.dumps({"type":"system","subtype":"init","session_id":session_id}) + "\n")
        f.write(json.dumps({"type":"user","message":{"role":"user","content":prompt}}) + "\n")
        f.write(json.dumps({"type":"assistant","message":{"model":"fake-model","content":[{"type":"text","text":response}]}}) + "\n")
        f.write(json.dumps({"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":False,"num_turns":1,"session_id":session_id,"result":response,"usage":{"input_tokens":1,"output_tokens":1}}) + "\n")
    sys.stdout.write("Context permissions /mcp\n")
    sys.stdout.flush()
    after = end + len(b"\x1b[201~")
    while after < len(buf) and buf[after:after + 1] in (b"\r", b"\n"):
        after += 1
    buf = buf[after:]
"#,
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
}
