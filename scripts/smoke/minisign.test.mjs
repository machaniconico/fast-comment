import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';

import { parsePublicKey, parseSignature, verifyMinisign } from './minisign.mjs';

function rawPublicKey(publicKey) {
  const der = publicKey.export({ format: 'der', type: 'spki' });
  return der.subarray(der.length - 32);
}

function buildSigned({ data, alg = 'Ed', keyId, publicKey, privateKey, trustedComment = 'timestamp:1700000000' }) {
  const rawPublic = rawPublicKey(publicKey);
  const pubText = `untrusted comment: test public key\n${Buffer.concat([
    Buffer.from('Ed', 'latin1'),
    keyId,
    rawPublic,
  ]).toString('base64')}\n`;

  const target = alg === 'ED' ? crypto.createHash('blake2b512').update(data).digest() : Buffer.from(data);
  const signatureBytes = crypto.sign(null, target, privateKey);
  const signatureLine = Buffer.concat([Buffer.from(alg, 'latin1'), keyId, signatureBytes]);
  const globalTarget = Buffer.concat([signatureBytes, Buffer.from(trustedComment, 'utf8')]);
  const globalSignature = crypto.sign(null, globalTarget, privateKey);
  const sigText = `untrusted comment: test signature\n${signatureLine.toString('base64')}\ntrusted comment: ${trustedComment}\n${globalSignature.toString('base64')}\n`;

  return {
    pubkeyB64: Buffer.from(pubText, 'utf8').toString('base64'),
    signatureB64: Buffer.from(sigText, 'utf8').toString('base64'),
    signatureBytes,
  };
}

function keypair() {
  return crypto.generateKeyPairSync('ed25519');
}

test('Ed 署名を検証できる', () => {
  const { publicKey, privateKey } = keypair();
  const data = Buffer.from('hello minisign');
  const signed = buildSigned({ data, alg: 'Ed', keyId: Buffer.from('0102030405060708', 'hex'), publicKey, privateKey });
  const result = verifyMinisign({ pubkeyB64: signed.pubkeyB64, signatureB64: signed.signatureB64, data });
  assert.deepEqual(result, { ok: true });
});

test('Ed 署名は改ざんで落ちる', () => {
  const { publicKey, privateKey } = keypair();
  const data = Buffer.from('hello minisign');
  const signed = buildSigned({ data, alg: 'Ed', keyId: Buffer.from('1112131415161718', 'hex'), publicKey, privateKey });
  const result = verifyMinisign({
    pubkeyB64: signed.pubkeyB64,
    signatureB64: signed.signatureB64,
    data: Buffer.from('hello minisign!'),
  });
  assert.equal(result.ok, false);
});

test('ED (BLAKE2b prehash) 署名を検証できる', () => {
  const { publicKey, privateKey } = keypair();
  const data = crypto.randomBytes(4096);
  const signed = buildSigned({ data, alg: 'ED', keyId: Buffer.from('2122232425262728', 'hex'), publicKey, privateKey });
  const result = verifyMinisign({ pubkeyB64: signed.pubkeyB64, signatureB64: signed.signatureB64, data });
  assert.deepEqual(result, { ok: true });
});

test('ED 署名は改ざんで落ちる', () => {
  const { publicKey, privateKey } = keypair();
  const data = Buffer.from('prehashed content');
  const signed = buildSigned({ data, alg: 'ED', keyId: Buffer.from('3132333435363738', 'hex'), publicKey, privateKey });
  const tampered = Buffer.from(data);
  tampered[0] ^= 0xff;
  const result = verifyMinisign({ pubkeyB64: signed.pubkeyB64, signatureB64: signed.signatureB64, data: tampered });
  assert.equal(result.ok, false);
});

test('keyId 不一致は失敗する', () => {
  const { publicKey, privateKey } = keypair();
  const data = Buffer.from('key id check');
  const signed = buildSigned({
    data,
    alg: 'Ed',
    keyId: Buffer.from('4142434445464748', 'hex'),
    publicKey,
    privateKey,
  });
  const other = parsePublicKey(signed.pubkeyB64);
  const wrongKeyId = Buffer.from(other.keyId);
  wrongKeyId[0] ^= 0xff;
  const rawPublic = rawPublicKey(publicKey);
  const pubText = `untrusted comment: other\n${Buffer.concat([
    Buffer.from('Ed', 'latin1'),
    wrongKeyId,
    rawPublic,
  ]).toString('base64')}\n`;
  const result = verifyMinisign({
    pubkeyB64: Buffer.from(pubText, 'utf8').toString('base64'),
    signatureB64: signed.signatureB64,
    data,
  });
  assert.equal(result.ok, false);
});

test('global signature の改ざんは失敗する', () => {
  const { publicKey, privateKey } = keypair();
  const data = Buffer.from('global signature check');
  const signed = buildSigned({ data, alg: 'Ed', keyId: Buffer.from('5152535455565758', 'hex'), publicKey, privateKey });
  const parsed = parseSignature(signed.signatureB64);
  const broken = Buffer.from(parsed.globalSignature);
  broken[0] ^= 0xff;
  const sigText = `untrusted comment: test signature\n${Buffer.concat([
    Buffer.from(parsed.alg, 'latin1'),
    parsed.keyId,
    parsed.signature,
  ]).toString('base64')}\ntrusted comment: ${parsed.trustedComment}\n${broken.toString('base64')}\n`;
  const result = verifyMinisign({
    pubkeyB64: signed.pubkeyB64,
    signatureB64: Buffer.from(sigText, 'utf8').toString('base64'),
    data,
  });
  assert.equal(result.ok, false);
});

test('parsePublicKey / parseSignature が各フィールドを返す', () => {
  const { publicKey, privateKey } = keypair();
  const keyId = Buffer.from('6162636465666768', 'hex');
  const signed = buildSigned({ data: Buffer.from('parse'), alg: 'Ed', keyId, publicKey, privateKey });
  const parsedPublic = parsePublicKey(signed.pubkeyB64);
  assert.equal(parsedPublic.alg, 'Ed');
  assert.ok(parsedPublic.keyId.equals(keyId));
  assert.equal(parsedPublic.publicKey.length, 32);

  const parsedSignature = parseSignature(signed.signatureB64);
  assert.equal(parsedSignature.alg, 'Ed');
  assert.ok(parsedSignature.keyId.equals(keyId));
  assert.equal(parsedSignature.signature.length, 64);
  assert.equal(parsedSignature.trustedComment, 'timestamp:1700000000');
  assert.equal(parsedSignature.globalSignature.length, 64);
});
