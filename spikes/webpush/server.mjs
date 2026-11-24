// Web Push 실기 검증용 서버(외부 패키지 없음, Node 내장 crypto만 사용).
// - RFC 8291(aes128gcm 암호화) + RFC 8292(VAPID) 직접 구현: 나중에 Rust로 옮길 로직과 같다.
// - 127.0.0.1에서만 듣는다. 폰 접속은 Tailscale Serve(tailnet 전용 HTTPS)가 넘겨준다.
// 사용: node server.mjs <데이터 폴더> [포트]
import http from "node:http";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const DATA = path.resolve(process.argv[2] ?? "./data");
const PORT = Number(process.argv[3] ?? 8766);
fs.mkdirSync(DATA, { recursive: true, mode: 0o700 });

const b64u = (buf) => Buffer.from(buf).toString("base64url");
const unb64u = (s) => Buffer.from(s, "base64url");
const log = (kind, data) => {
  const line = JSON.stringify({ t: Date.now(), kind, ...data });
  fs.appendFileSync(path.join(DATA, "log.jsonl"), line + "\n");
  console.log(line);
};

// ---- VAPID 키(한 번 만들고 파일 600으로 보관) ----
const keyFile = path.join(DATA, "vapid.json");
if (!fs.existsSync(keyFile)) {
  const { privateKey } = crypto.generateKeyPairSync("ec", { namedCurve: "prime256v1" });
  fs.writeFileSync(keyFile, JSON.stringify(privateKey.export({ format: "jwk" })), { mode: 0o600 });
}
const vapidJwk = JSON.parse(fs.readFileSync(keyFile, "utf8"));
const vapidPrivate = crypto.createPrivateKey({ key: vapidJwk, format: "jwk" });
// 비압축 공개키 65바이트 = 0x04 || x || y
const vapidPublicRaw = Buffer.concat([Buffer.from([4]), unb64u(vapidJwk.x), unb64u(vapidJwk.y)]);

function vapidHeader(endpoint) {
  const aud = new URL(endpoint).origin;
  const header = b64u(JSON.stringify({ typ: "JWT", alg: "ES256" }));
  // sub에는 실제 연락처를 넣지 않는다(검증용 자리표시).
  const claims = b64u(JSON.stringify({ aud, exp: Math.floor(Date.now() / 1000) + 12 * 3600, sub: "mailto:push-test@example.invalid" }));
  const signingInput = `${header}.${claims}`;
  const sig = crypto.sign("sha256", Buffer.from(signingInput), { key: vapidPrivate, dsaEncoding: "ieee-p1363" });
  return `vapid t=${signingInput}.${b64u(sig)}, k=${b64u(vapidPublicRaw)}`;
}

// ---- RFC 8291 aes128gcm 암호화 ----
export function encrypt(payload, uaPublicB64, authB64) {
  const uaPublic = unb64u(uaPublicB64); // 폰(브라우저)의 p256dh 공개키 65바이트
  const authSecret = unb64u(authB64); // 16바이트
  const ecdh = crypto.createECDH("prime256v1");
  const asPublic = ecdh.generateKeys(); // 발송마다 새 임시 키
  const ecdhSecret = ecdh.computeSecret(uaPublic);
  const keyInfo = Buffer.concat([Buffer.from("WebPush: info\0"), uaPublic, asPublic]);
  const ikm = Buffer.from(crypto.hkdfSync("sha256", ecdhSecret, authSecret, keyInfo, 32));
  const salt = crypto.randomBytes(16);
  const cek = Buffer.from(crypto.hkdfSync("sha256", ikm, salt, Buffer.from("Content-Encoding: aes128gcm\0"), 16));
  const nonce = Buffer.from(crypto.hkdfSync("sha256", ikm, salt, Buffer.from("Content-Encoding: nonce\0"), 12));
  const cipher = crypto.createCipheriv("aes-128-gcm", cek, nonce);
  // 마지막(유일한) 레코드 구분자 0x02
  const body = Buffer.concat([cipher.update(Buffer.concat([Buffer.from(payload), Buffer.from([2])])), cipher.final(), cipher.getAuthTag()]);
  const rs = Buffer.alloc(4);
  rs.writeUInt32BE(4096);
  return Buffer.concat([salt, rs, Buffer.from([asPublic.length]), asPublic, body]);
}

// ---- 구독 저장(검증용: 파일 하나) ----
const subFile = path.join(DATA, "subscriptions.json");
const loadSubs = () => (fs.existsSync(subFile) ? JSON.parse(fs.readFileSync(subFile, "utf8")) : []);
const saveSubs = (subs) => fs.writeFileSync(subFile, JSON.stringify(subs, null, 2), { mode: 0o600 });

async function sendOne(sub, msg, { ttl = 300, urgency = "high", topic } = {}) {
  const body = encrypt(JSON.stringify(msg), sub.keys.p256dh, sub.keys.auth);
  const headers = {
    Authorization: vapidHeader(sub.endpoint),
    "Content-Encoding": "aes128gcm",
    "Content-Type": "application/octet-stream",
    TTL: String(ttl),
    Urgency: urgency,
  };
  if (topic) headers.Topic = topic;
  const t0 = Date.now();
  const res = await fetch(sub.endpoint, { method: "POST", headers, body });
  const text = await res.text();
  log("sent", { id: msg.id, status: res.status, ms: Date.now() - t0, host: new URL(sub.endpoint).host, resp: text.slice(0, 200) });
  return res.status;
}

async function sendAll(msg, opts) {
  const subs = loadSubs();
  const keep = [];
  for (const s of subs) {
    const st = await sendOne(s, msg, opts);
    if (st !== 404 && st !== 410) keep.push(s); // 만료 구독은 지운다(T29)
  }
  if (keep.length !== subs.length) saveSubs(keep);
  return subs.length;
}

// ---- 정적 파일 ----
const PAGE = `<!doctype html><html lang="ko"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="manifest" href="/manifest.json"><meta name="theme-color" content="#1f2937">
<title>푸시 시험</title>
<style>body{font:16px/1.5 system-ui,sans-serif;margin:24px;max-width:520px}button{font-size:17px;padding:12px 16px;margin:6px 0;width:100%;border-radius:10px;border:1px solid #888;background:#f3f4f6}
#s{white-space:pre-wrap;background:#111827;color:#e5e7eb;padding:12px;border-radius:10px;font:13px/1.5 ui-monospace,monospace}</style></head><body>
<h2>AI 작업 알림 · 푸시 시험</h2>
<p>1) 알림 허용 → 2) 시험 알림 보내기를 누른 뒤 화면을 끄거나 앱을 닫고 기다려 보세요.</p>
<button id="sub">1. 알림 허용하고 구독</button>
<button id="t5">2. 5초 뒤 시험 알림</button>
<button id="t30">3. 30초 뒤 시험 알림 (화면 끄고 대기)</button>
<button id="un">구독 해제</button>
<div id="s">상태 확인 중…</div>
<script>
const S=document.getElementById('s');const say=(m)=>{S.textContent=new Date().toLocaleTimeString()+'  '+m+'\\n'+S.textContent;fetch('/clientlog',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({m:String(m)})}).catch(()=>{})};
window.addEventListener('error',(e)=>say('JS 오류: '+e.message));window.addEventListener('unhandledrejection',(e)=>say('Promise 오류: '+e.reason));
const u8=(b)=>{const p='='.repeat((4-b.length%4)%4);const r=atob((b+p).replace(/-/g,'+').replace(/_/g,'/'));return Uint8Array.from(r,c=>c.charCodeAt(0))};
async function reg(){return navigator.serviceWorker.register('/sw.js')}
async function status(){
  if(!('serviceWorker' in navigator)||!('PushManager' in window)){say('이 브라우저는 Web Push를 지원하지 않음');return}
  const r=await reg();const s=await r.pushManager.getSubscription();
  say('권한='+Notification.permission+' 구독='+(s?'있음':'없음')+' 설치형='+(matchMedia('(display-mode: standalone)').matches?'예':'아니요')+'\\n'+navigator.userAgent);
}
document.getElementById('sub').onclick=async()=>{
  try{say('권한 요청 중');const p=await Notification.requestPermission();say('권한 결과: '+p);if(p!=='granted'){say('권한 거부됨: '+p);return}
  say('서비스 워커 등록 중');const r=await reg();await navigator.serviceWorker.ready;say('서비스 워커 준비됨');
  const key=await (await fetch('/vapid')).text();
  say('푸시 구독 요청 중');const s=await r.pushManager.subscribe({userVisibleOnly:true,applicationServerKey:u8(key)});
  const res=await fetch('/subscribe',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(s)});
  say('구독 완료 ('+res.status+') '+new URL(s.endpoint).host)}catch(e){say('오류: '+(e&&e.name)+' '+(e&&e.message))}
};
const test=(d)=>async()=>{const r=await fetch('/send?delay='+d,{method:'POST'});say(d+'초 뒤 발송 예약: '+(await r.text()))};
document.getElementById('t5').onclick=test(5);document.getElementById('t30').onclick=test(30);
document.getElementById('un').onclick=async()=>{const r=await reg();const s=await r.pushManager.getSubscription();if(s){await s.unsubscribe();await fetch('/unsubscribe',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({endpoint:s.endpoint})})}say('구독 해제')};
const q=new URLSearchParams(location.search);if(q.get('opened'))say('알림을 눌러 열림: '+q.get('opened'));
status();
</script></body></html>`;

const SW = `
self.addEventListener('install',()=>self.skipWaiting());
self.addEventListener('activate',(e)=>e.waitUntil(self.clients.claim()));
self.addEventListener('push',(e)=>{
  const recv=Date.now();let m={};try{m=e.data.json()}catch(_){m={title:'AI 작업 알림',body:'(내용 해석 실패)'}}
  e.waitUntil(Promise.all([
    self.registration.showNotification(m.title||'AI 작업 알림',{body:m.body||'',tag:m.tag||m.id,renotify:!!m.renotify,silent:!!m.silent,data:{id:m.id,url:'/?opened='+encodeURIComponent(m.id||'')},icon:'/icon.svg',badge:'/icon.svg'}),
    fetch('/ack',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({id:m.id,sentAt:m.sentAt,recvAt:recv,visible:self.clients?undefined:undefined})}).catch(()=>{})
  ]));
});
self.addEventListener('notificationclick',(e)=>{
  e.notification.close();const url=(e.notification.data&&e.notification.data.url)||'/';
  e.waitUntil((async()=>{
    fetch('/ack',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({id:e.notification.data&&e.notification.data.id,clickAt:Date.now()})}).catch(()=>{});
    const cs=await clients.matchAll({type:'window',includeUncontrolled:true});
    for(const c of cs){if('focus' in c){await c.navigate(url).catch(()=>{});return c.focus()}}
    return clients.openWindow(url);
  })());
});
self.addEventListener('pushsubscriptionchange',(e)=>{
  fetch('/ack',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({event:'pushsubscriptionchange'})}).catch(()=>{});
});`;

const MANIFEST = JSON.stringify({
  name: "AI 작업 알림(시험)", short_name: "AI 알림", start_url: "/", scope: "/", display: "standalone",
  background_color: "#ffffff", theme_color: "#1f2937",
  icons: [{ src: "/icon.svg", sizes: "any", type: "image/svg+xml", purpose: "any" }],
});
const ICON = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 96 96"><rect width="96" height="96" rx="20" fill="#1f2937"/><circle cx="48" cy="48" r="22" fill="none" stroke="#fff" stroke-width="8"/><circle cx="48" cy="48" r="6" fill="#fff"/></svg>`;

let seq = 0;
async function readJson(req) {
  let raw = "";
  for await (const c of req) { raw += c; if (raw.length > 16384) throw new Error("too large"); }
  return JSON.parse(raw || "{}");
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, "http://x");
  const send = (code, type, body, extra = {}) => { res.writeHead(code, { "content-type": type, "cache-control": "no-store", ...extra }); res.end(body); };
  if (url.pathname !== "/clientlog") log("req", { m: req.method, p: url.pathname });
  try {
    if (req.method === "GET" && url.pathname === "/") return send(200, "text/html; charset=utf-8", PAGE);
    if (req.method === "GET" && url.pathname === "/sw.js") return send(200, "text/javascript; charset=utf-8", SW);
    if (req.method === "GET" && url.pathname === "/manifest.json") return send(200, "application/manifest+json", MANIFEST);
    if (req.method === "GET" && url.pathname === "/icon.svg") return send(200, "image/svg+xml", ICON);
    if (req.method === "GET" && url.pathname === "/vapid") return send(200, "text/plain", b64u(vapidPublicRaw));
    if (req.method === "POST" && url.pathname === "/subscribe") {
      const s = await readJson(req);
      if (!s.endpoint || !s.keys?.p256dh || !s.keys?.auth) return send(400, "text/plain", "bad");
      const subs = loadSubs().filter((x) => x.endpoint !== s.endpoint);
      subs.push({ endpoint: s.endpoint, keys: s.keys, at: Date.now(), ua: req.headers["user-agent"] });
      saveSubs(subs);
      log("subscribe", { host: new URL(s.endpoint).host, count: subs.length, ua: req.headers["user-agent"] });
      return send(201, "text/plain", "ok");
    }
    if (req.method === "POST" && url.pathname === "/unsubscribe") {
      const s = await readJson(req);
      saveSubs(loadSubs().filter((x) => x.endpoint !== s.endpoint));
      log("unsubscribe", {});
      return send(200, "text/plain", "ok");
    }
    if (req.method === "POST" && url.pathname === "/send") {
      const delay = Math.min(Math.max(Number(url.searchParams.get("delay") ?? 0), 0), 600);
      const id = `t${Date.now()}-${++seq}`;
      setTimeout(() => {
        const msg = { id, title: "AI 작업에 결정이 필요해요", body: "시험 알림입니다", tag: id, sentAt: Date.now() };
        sendAll(msg).catch((e) => log("send_error", { id, err: String(e) }));
      }, delay * 1000);
      const n = loadSubs().length;
      log("scheduled", { id, delay, subs: n });
      return send(202, "text/plain", n ? `${id} (구독 ${n}개)` : "구독이 없어 보낼 곳이 없음 — 먼저 1번을 눌러 주세요");
    }
    if (req.method === "POST" && url.pathname === "/clientlog") {
      const a = await readJson(req);
      log("client", { m: String(a.m).slice(0, 500), ua: req.headers["user-agent"] });
      return send(204, "text/plain", "");
    }
    if (req.method === "POST" && url.pathname === "/ack") {
      const a = await readJson(req);
      const extra = a.recvAt && a.sentAt ? { latencyMs: a.recvAt - a.sentAt } : {};
      log("ack", { ...a, ...extra });
      return send(204, "text/plain", "");
    }
    send(404, "text/plain", "not found");
  } catch (e) {
    log("error", { path: url.pathname, err: String(e) });
    send(500, "text/plain", "error");
  }
});
export { vapidHeader, vapidPublicRaw };
if (!process.env.NO_LISTEN) server.listen(PORT, "127.0.0.1", () => console.log(`listening 127.0.0.1:${PORT} data=${DATA}`));
