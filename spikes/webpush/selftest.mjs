// 수신 측(브라우저) 복호화를 RFC 8291대로 따로 구현해 왕복을 확인하고, VAPID JWT 서명을 검증한다.
import crypto from "node:crypto";
import assert from "node:assert/strict";
process.env.NO_LISTEN = "1";
const { encrypt, vapidHeader, vapidPublicRaw } = await import("./server.mjs");

const ua = crypto.createECDH("prime256v1");
const uaPublic = ua.generateKeys();
const auth = crypto.randomBytes(16);
const msg = JSON.stringify({ id: "x1", title: "한글 제목", body: "본문".repeat(50) });
const enc = encrypt(msg, uaPublic.toString("base64url"), auth.toString("base64url"));

// 헤더 파싱
const salt = enc.subarray(0, 16);
const rs = enc.readUInt32BE(16);
const idlen = enc[20];
const asPublic = enc.subarray(21, 21 + idlen);
const ct = enc.subarray(21 + idlen);
assert.equal(rs, 4096);
assert.equal(idlen, 65);
const secret = ua.computeSecret(asPublic);
const ikm = Buffer.from(crypto.hkdfSync("sha256", secret, auth, Buffer.concat([Buffer.from("WebPush: info\0"), uaPublic, asPublic]), 32));
const cek = Buffer.from(crypto.hkdfSync("sha256", ikm, salt, Buffer.from("Content-Encoding: aes128gcm\0"), 16));
const nonce = Buffer.from(crypto.hkdfSync("sha256", ikm, salt, Buffer.from("Content-Encoding: nonce\0"), 12));
const d = crypto.createDecipheriv("aes-128-gcm", cek, nonce);
d.setAuthTag(ct.subarray(ct.length - 16));
const plain = Buffer.concat([d.update(ct.subarray(0, ct.length - 16)), d.final()]);
assert.equal(plain[plain.length - 1], 2, "마지막 레코드 구분자 0x02");
assert.equal(plain.subarray(0, -1).toString(), msg);
console.log("암호화 왕복 OK, 길이", enc.length);

// 잘못된 auth로는 풀리지 않아야 함
assert.throws(() => {
  const bad = Buffer.from(crypto.hkdfSync("sha256", secret, crypto.randomBytes(16), Buffer.concat([Buffer.from("WebPush: info\0"), uaPublic, asPublic]), 32));
  const k = Buffer.from(crypto.hkdfSync("sha256", bad, salt, Buffer.from("Content-Encoding: aes128gcm\0"), 16));
  const n = Buffer.from(crypto.hkdfSync("sha256", bad, salt, Buffer.from("Content-Encoding: nonce\0"), 12));
  const dd = crypto.createDecipheriv("aes-128-gcm", k, n);
  dd.setAuthTag(ct.subarray(ct.length - 16));
  dd.update(ct.subarray(0, ct.length - 16)); dd.final();
});
console.log("잘못된 auth 거부 OK");

// VAPID
const h = vapidHeader("https://fcm.googleapis.com/fcm/send/abc");
const m = h.match(/^vapid t=([^,]+), k=(.+)$/);
const [hd, cl, sig] = m[1].split(".");
const claims = JSON.parse(Buffer.from(cl, "base64url"));
assert.equal(claims.aud, "https://fcm.googleapis.com");
assert.ok(claims.exp > Date.now() / 1000 && claims.exp <= Date.now() / 1000 + 24 * 3600, "exp는 24시간 이내");
const pub = Buffer.from(m[2], "base64url");
assert.deepEqual(pub, vapidPublicRaw);
const jwk = { kty: "EC", crv: "P-256", x: pub.subarray(1, 33).toString("base64url"), y: pub.subarray(33).toString("base64url") };
const ok = crypto.verify("sha256", Buffer.from(`${hd}.${cl}`), { key: crypto.createPublicKey({ key: jwk, format: "jwk" }), dsaEncoding: "ieee-p1363" }, Buffer.from(sig, "base64url"));
assert.ok(ok, "VAPID 서명 검증");
console.log("VAPID 서명 OK", claims);
