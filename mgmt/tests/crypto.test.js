// node mgmt/tests/crypto.test.js — static/crypto.js (the browser side of sign-in) gives the keys and the sealed
// password the server expects: the same vectors as tests/unit/pwproof.rs same_as_the_browser.
'use strict';
const fs = require('fs'), path = require('path'), vm = require('vm'), assert = require('assert');

// The seal's random salt is fixed, so its output is the exact Sealed that pwproof.rs pins.
const SALT = '965ea71cf5e3b293335647a59f71aea1';
const ctx = vm.createContext({
  TextEncoder,
  crypto: { getRandomValues: a => { a.set(Buffer.from(SALT, 'hex')); return a; } },
});
const src = fs.readFileSync(path.join(__dirname, '../static/crypto.js'), 'utf8');
const { pwKeys, setupKey, sealPw, hexOf } = vm.runInContext(`${src}\n;({pwKeys, setupKey, sealPw, hexOf})`, ctx);

const k = pwKeys('correct horse', Buffer.from('0123456789abcdef').toString('hex'), 1000);
assert.deepStrictEqual([hexOf(k.client), hexOf(k.stored), hexOf(k.server)], [
  '782b833d6612b1b7cd9e8681e9b7e503aaa94969c7aa3f13ff44bb6032691f56',
  '5ab388351aa7a2f880b27b89ca9da7745b29e0418ede015eb5e5a1210ca35af8',
  'dc742ef7eade29afea76b33ac0493f4539ce03e54dabde825d7342a123308539',
]);
assert.strictEqual(hexOf(setupKey(' tok ')), '7f13e102df9371951601b44031b8205aa178f5eb8ce928fa20987369b43507a7');
assert.deepStrictEqual({ ...sealPw(new Uint8Array(32).fill(7), 'staff1', 'new password') }, {
  salt: SALT,
  iter: 600000,
  stored: 'aeee1b27a0346d1ca7865dce8e35cf45cdd79c9944c512fc232468249767ea01',
  server: '921e3c70dee1d2a5dcf40f988f382efe0b394337526cc1ff78ee5231edd048a7',
  tag: '1ed5897ba161511104cf57d35a19e2bc3c062f40920b3faaca19ca9c974b72d0',
});
console.log('crypto.js: keys, setup key and seal match pwproof.rs');
