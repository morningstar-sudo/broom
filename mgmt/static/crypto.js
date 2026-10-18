// crypto.js — the browser side of the sign-in handshake (pwproof.rs on the server): the password never leaves the
// page. SHA-256, HMAC and PBKDF2 in plain JS: browsers turn WebCrypto (crypto.subtle) off on plain http:// to an IP
// (not a "secure context"); crypto.getRandomValues still works there.
'use strict';
const SHA_K=new Uint32Array([0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
  0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,0xe49b69c1,0xefbe4786,
  0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,
  0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,
  0x81c2c92e,0x92722c85,0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
  0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,0x748f82ee,0x78a5636f,
  0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2]);
const SHA_H0=[0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19];
// One SHA-256 block: state h (8 words) updated by w (64 words, the first 16 = the block, big-endian).
function shaBlock(h,w){
  for(let i=16;i<64;i++){const a=w[i-15],b=w[i-2];
    w[i]=(w[i-16]+(((a>>>7)|(a<<25))^((a>>>18)|(a<<14))^(a>>>3))+w[i-7]+(((b>>>17)|(b<<15))^((b>>>19)|(b<<13))^(b>>>10)))|0;}
  let a=h[0],b=h[1],c=h[2],d=h[3],e=h[4],f=h[5],g=h[6],k=h[7];
  for(let i=0;i<64;i++){
    const t1=(k+(((e>>>6)|(e<<26))^((e>>>11)|(e<<21))^((e>>>25)|(e<<7)))+((e&f)^(~e&g))+SHA_K[i]+w[i])|0;
    const t2=((((a>>>2)|(a<<30))^((a>>>13)|(a<<19))^((a>>>22)|(a<<10)))+((a&b)^(a&c)^(b&c)))|0;
    k=g;g=f;f=e;e=(d+t1)|0;d=c;c=b;b=a;a=(t1+t2)|0;}
  h[0]=(h[0]+a)|0;h[1]=(h[1]+b)|0;h[2]=(h[2]+c)|0;h[3]=(h[3]+d)|0;h[4]=(h[4]+e)|0;h[5]=(h[5]+f)|0;h[6]=(h[6]+g)|0;h[7]=(h[7]+k)|0;}
function shaBytes(h){const o=new Uint8Array(32),v=new DataView(o.buffer);for(let i=0;i<8;i++)v.setUint32(i*4,h[i]);return o;}
function sha256(m){
  const n=m.length,b=new Uint8Array(((n+9+63)>>6)*64);b.set(m);b[n]=0x80;
  const v=new DataView(b.buffer);v.setUint32(b.length-4,n*8);v.setUint32(b.length-8,Math.floor(n/0x20000000));
  const h=Uint32Array.from(SHA_H0),w=new Uint32Array(64);
  for(let o=0;o<b.length;o+=64){for(let i=0;i<16;i++)w[i]=v.getUint32(o+i*4);shaBlock(h,w);}
  return shaBytes(h);}
const cat=(...a)=>{const o=new Uint8Array(a.reduce((n,x)=>n+x.length,0));let p=0;for(const x of a){o.set(x,p);p+=x.length;}return o;};
function hmac(k,m){if(k.length>64)k=sha256(k);const ip=new Uint8Array(64),op=new Uint8Array(64);ip.set(k);op.set(k);
  for(let i=0;i<64;i++){ip[i]^=0x36;op[i]^=0x5c;}return sha256(cat(op,sha256(cat(ip,m))));}
// PBKDF2-HMAC-SHA256, 32 bytes. After the first round each one is 2 blocks from the keyed inner / outer states.
function pbkdf2(pw,salt,iter){
  const key=pw.length>64?sha256(pw):pw;
  const keyed=x=>{const b=new Uint8Array(64);b.set(key);for(let i=0;i<64;i++)b[i]^=x;
    const h=Uint32Array.from(SHA_H0),w=new Uint32Array(64),v=new DataView(b.buffer);for(let i=0;i<16;i++)w[i]=v.getUint32(i*4);shaBlock(h,w);return h;};
  const ist=keyed(0x36),ost=keyed(0x5c),w=new Uint32Array(64),h=new Uint32Array(8);
  const tail=(st,x)=>{h.set(st);w.set(x);w[8]=0x80000000;w.fill(0,9,15);w[15]=(64+32)*8;shaBlock(h,w);return h;};
  const u1=hmac(pw,cat(salt,new Uint8Array([0,0,0,1]))),uv=new DataView(u1.buffer);
  const u=new Uint32Array(8);for(let i=0;i<8;i++)u[i]=uv.getUint32(i*4);
  const t=Uint32Array.from(u);
  for(let r=1;r<iter;r++){const inner=Uint32Array.from(tail(ist,u));u.set(tail(ost,inner));for(let i=0;i<8;i++)t[i]^=u[i];}
  return shaBytes(t);}

// ---- the handshake (same names and messages as pwproof.rs)
const PW_ITER=600000;
const utf8=s=>new TextEncoder().encode(s);
const hexOf=b=>Array.from(b,x=>x.toString(16).padStart(2,'0')).join('');
const unhex=s=>new Uint8Array((s.match(/../g)||[]).map(x=>parseInt(x,16)));
const xor32=(a,b)=>a.map((x,i)=>x^b[i]);
const randHex=n=>hexOf(crypto.getRandomValues(new Uint8Array(n)));
// (ClientKey, stored, server) of a password.
function pwKeys(pw,saltHex,iter){const k=pbkdf2(utf8(pw),unhex(saltHex),iter),client=hmac(k,utf8('client'));
  return {client,stored:sha256(client),server:hmac(k,utf8('server'))};}
async function authPost(url,body){const r=await fetch(url,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});
  if(!r.ok)throw new Error((await r.text())||('HTTP '+r.status));return r.json();}
// Steps 1–2 (hello → salt + server nonce) and the proof of step 3; `done(server_proof)` checks step 4.
async function handshake(user,pw){
  const cnonce=randHex(16),h=await authPost('/api/auth/hello',{user,cnonce});
  if(!(h.iter>=100000&&h.iter<=10000000))throw new Error('unexpected answer from the server');
  const k=pwKeys(pw,h.salt,h.iter),t=utf8(`broom-login|${user}|${cnonce}|${h.snonce}`);
  return {hs:h.hs,proof:hexOf(xor32(k.client,hmac(k.stored,t))),sk:hmac(k.client,cat(utf8('session|'),t)),
    serverOk:p=>p===hexOf(hmac(k.server,t))};}
// A new password for `user` as the server takes it: its verifier masked with `key` (the session key, or the setup
// token's), tagged.
function sealPw(key,user,pw){
  const salt=randHex(16),iter=PW_ITER,k=pwKeys(pw,salt,iter),l=`${user}|${salt}|${iter}`;
  const stored=hexOf(xor32(k.stored,hmac(key,utf8('stored|'+l)))),server=hexOf(xor32(k.server,hmac(key,utf8('server|'+l))));
  return {salt,iter,stored,server,tag:hexOf(hmac(key,utf8(`cred|${l}|${stored}|${server}`)))};}
const setupKey=token=>hmac(utf8(token.trim()),utf8('broom-setup'));
// The session key of this browser's session (what new passwords are sealed with), kept until logout.
const SK_KEY='broom_sk';
function keepSk(sk){try{localStorage.setItem(SK_KEY,hexOf(sk));}catch(e){}}
function sessionSk(){try{const s=localStorage.getItem(SK_KEY);return s?unhex(s):null;}catch(e){return null;}}
// Sign in: the whole handshake. Throws on a wrong password, or when the server can't prove it holds the account.
async function signIn(user,pw){
  const h=await handshake(user,pw),r=await authPost('/api/auth/login',{hs:h.hs,proof:h.proof});
  if(!h.serverOk(r.server_proof))throw new Error('the server could not prove it holds this account');
  keepSk(h.sk);}
