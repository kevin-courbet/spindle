const ws = new WebSocket("ws://127.0.0.1:19990");
let nextID = 0;
const pending = new Map();
let acpSessionID = null;
let sessionReadyResolve = null;

ws.onmessage = (ev) => {
  if (typeof ev.data === "string") {
    const d = JSON.parse(ev.data);
    if (d.id && pending.has(d.id)) { pending.get(d.id)(d); pending.delete(d.id); return; }
    if (d.method === "chat.session_ready" && sessionReadyResolve) {
      acpSessionID = d.params?.acp_session_id;
      sessionReadyResolve(d); sessionReadyResolve = null;
    }
  } else {
    const buf = Buffer.from(ev.data);
    const payload = buf.subarray(2).toString("utf-8").trim();
    if (!payload) return;
    try {
      const parsed = JSON.parse(payload);
      if (parsed.method === "session/update") {
        const u = parsed.params?.update;
        const t = u?.type || "?";
        if (t === "agent_message_chunk") process.stdout.write(u.text || "");
        else if (t === "agent_thought_chunk") process.stderr.write(".");
        else if (t !== "user_message_chunk") console.log("\n  [" + t + "]");
      } else if (parsed.id && pending.has(parsed.id)) {
        pending.get(parsed.id)(parsed); pending.delete(parsed.id);
      }
    } catch {}
  }
};

function rpc(m, p) { return new Promise(r => { const i=++nextID; pending.set(i,r); ws.send(JSON.stringify({jsonrpc:"2.0",id:i,method:m,params:p})); }); }
function fail(msg) { console.error("\nFAIL: " + msg); ws.close(); process.exit(1); }
function sendBinaryRPC(ch, method, params) {
  return new Promise(resolve => {
    const id = ++nextID; pending.set(id, resolve);
    const header = Buffer.alloc(2); header.writeUInt16BE(ch);
    ws.send(Buffer.concat([header, Buffer.from(JSON.stringify({jsonrpc:"2.0",id,method,params}) + "\n")]));
  });
}

ws.onopen = async () => {
  const t0 = Date.now();
  const e = () => (Date.now()-t0)+"ms";

  await rpc("session.hello", {client:{name:"test",version:"1.0"}, protocol_version:"2026-03-17", capabilities:["state.delta.operations.v1","preset.output.v1","rpc.errors.structured.v1"]});
  console.log("["+e()+"] hello");

  const projects = (await rpc("project.list",{})).result||[];
  const proj = projects.find(p=>p.agents?.some(a=>a.name==="opencode"));
  if(!proj) fail("no project with opencode");
  const threads = (await rpc("thread.list",{project_id:proj.id})).result||[];
  const thread = threads.find(t=>t.status==="active");
  if(!thread) fail("no active thread");
  console.log("["+e()+"] thread: "+thread.name);

  const readyP = new Promise(r=>{sessionReadyResolve=r;});
  const start = await rpc("chat.start",{thread_id:thread.id,agent_name:"opencode"});
  if(start.error) fail("chat.start: "+JSON.stringify(start.error));
  console.log("["+e()+"] started: "+start.result.session_id);
  await readyP;
  console.log("["+e()+"] ready acp="+acpSessionID);

  const attach = await rpc("chat.attach",{thread_id:thread.id,session_id:start.result.session_id});
  if(attach.error) fail("attach: "+JSON.stringify(attach.error));
  const ch = attach.result.channel_id;
  const acp = attach.result.acp_session_id;
  console.log("["+e()+"] attached ch="+ch);

  // Switch to Haiku for fast responses
  console.log("["+e()+"] switching to haiku...");
  const setModel = await sendBinaryRPC(ch, "session/set_model", {sessionId: acp, modelId: "anthropic/claude-3-5-haiku-latest"});
  if(setModel.error) console.log("["+e()+"] setModel error (non-fatal): "+JSON.stringify(setModel.error));
  else console.log("["+e()+"] model set to haiku");

  // Send prompt
  const nonce = Math.random().toString(36).slice(2,10);
  console.log("["+e()+"] sending prompt...");
  const resp = await sendBinaryRPC(ch, "session/prompt", {sessionId: acp, prompt: [{type:"text", text:"Reply with exactly one word: ACK-"+nonce}]});
  if(resp.error) fail("prompt: "+JSON.stringify(resp.error));
  console.log("\n["+e()+"] response (stop="+resp.result?.stopReason+")");

  await rpc("chat.stop",{thread_id:thread.id,session_id:start.result.session_id});
  console.log("["+e()+"] done\nPASS");
  ws.close(); process.exit(0);
};
ws.onerror=()=>fail("ws error");
setTimeout(()=>fail("timeout 30s"),30000);
