import crypto from 'node:crypto';

const SPKI_PREFIX = Buffer.from('302a300506032b6570032100', 'hex');
const PUBLIC_KEY_LEN = 42;
const SIGNATURE_LEN = 74;
const GLOBAL_SIGNATURE_LEN = 64;

function decodeBase64(text) {
  return Buffer.from(String(text).trim(), 'base64');
}

function textLines(buf) {
  return buf
    .toString('utf8')
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith('untrusted comment:') && !line.startsWith('trusted comment:'));
}

function findDecodedLine(buf, length) {
  const direct = decodeBase64(buf.toString('utf8'));
  if (direct.length === length) return direct;
  for (const line of textLines(buf)) {
    const decoded = decodeBase64(line);
    if (decoded.length === length) return decoded;
  }
  return null;
}

export function parsePublicKey(pubkeyB64) {
  const decoded = decodeBase64(pubkeyB64);
  const raw = decoded.length === PUBLIC_KEY_LEN ? decoded : findDecodedLine(decoded, PUBLIC_KEY_LEN);
  if (!raw) throw new Error('公開鍵の base64 行が見つかりません');
  const alg = raw.toString('latin1', 0, 2);
  if (alg !== 'Ed') throw new Error(`公開鍵の alg が不正です: ${alg}`);
  return {
    alg,
    keyId: raw.subarray(2, 10),
    publicKey: raw.subarray(10, PUBLIC_KEY_LEN),
  };
}

export function parseSignature(signatureB64) {
  const decoded = decodeBase64(signatureB64);
  const raw = decoded.length === SIGNATURE_LEN ? decoded : findDecodedLine(decoded, SIGNATURE_LEN);
  if (!raw) throw new Error('署名の base64 行が見つかりません');
  const alg = raw.toString('latin1', 0, 2);
  if (alg !== 'Ed' && alg !== 'ED') throw new Error(`署名の alg が不正です: ${alg}`);

  const text = decoded.length === SIGNATURE_LEN ? '' : decoded.toString('utf8');
  const lines = text.split(/\r?\n/);
  let trustedComment = null;
  let globalSignature = null;
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (line.startsWith('trusted comment: ')) {
      // 前後の空白も署名対象なので trim しない(改行は split で除去済み)
      trustedComment = line.slice('trusted comment: '.length);
      for (let j = i + 1; j < lines.length; j += 1) {
        const candidate = lines[j].trim();
        if (!candidate) continue;
        const buf = decodeBase64(candidate);
        if (buf.length === GLOBAL_SIGNATURE_LEN) {
          globalSignature = buf;
          break;
        }
      }
      break;
    }
  }

  return {
    alg,
    keyId: raw.subarray(2, 10),
    signature: raw.subarray(10, SIGNATURE_LEN),
    trustedComment,
    globalSignature,
  };
}

export function verifyMinisign({ pubkeyB64, signatureB64, data } = {}) {
  try {
    const publicKey = parsePublicKey(pubkeyB64);
    const signature = parseSignature(signatureB64);

    if (!publicKey.keyId.equals(signature.keyId)) {
      return { ok: false, reason: 'keyId が一致しません' };
    }

    const body = Buffer.isBuffer(data) ? data : Buffer.from(data ?? '');
    const target = signature.alg === 'ED' ? crypto.createHash('blake2b512').update(body).digest() : body;
    const key = crypto.createPublicKey({
      key: Buffer.concat([SPKI_PREFIX, publicKey.publicKey]),
      format: 'der',
      type: 'spki',
    });

    if (!crypto.verify(null, target, key, signature.signature)) {
      return { ok: false, reason: '本文の署名が一致しません' };
    }

    if (!signature.globalSignature || signature.trustedComment === null) {
      return { ok: false, reason: 'global signature または trusted comment がありません' };
    }

    const globalTarget = Buffer.concat([
      signature.signature,
      Buffer.from(signature.trustedComment, 'utf8'),
    ]);
    if (!crypto.verify(null, globalTarget, key, signature.globalSignature)) {
      return { ok: false, reason: 'global signature が一致しません' };
    }

    return { ok: true };
  } catch (error) {
    return { ok: false, reason: error instanceof Error ? error.message : String(error) };
  }
}
