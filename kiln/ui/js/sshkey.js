// SSH key pairs made in the browser, so the private key never leaves this
// machine: kiln only ever gets the public half. Browsers only offer their
// own crypto to HTTPS pages and kiln serves plain HTTP, so Ed25519 comes
// from TweetNaCl (vendor/nacl.min.js), loaded the first time a key is made.

let loading;
function nacl() {
  loading ??= new Promise((resolve, reject) => {
    const failed = () => { loading = undefined; reject(new Error("couldn't load the key library")); };
    const s = Object.assign(document.createElement('script'), { src: '/ui/vendor/nacl.min.js' });
    s.addEventListener('load', () => (window.nacl ? resolve(window.nacl) : failed()));
    s.addEventListener('error', failed);
    document.head.append(s);
  });
  return loading;
}

const bytes = (s) => new TextEncoder().encode(s);
const u32 = (n) => new Uint8Array([n >>> 24, (n >>> 16) & 255, (n >>> 8) & 255, n & 255]);
const cat = (...parts) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
};
/** An SSH "string": its length, then its bytes. */
const str = (b) => cat(u32(b.length), b);
const b64 = (b) => btoa(String.fromCharCode(...b));

/** A new Ed25519 pair: `publicKey` as one authorized_keys line, and
 *  `privateKey` as the file `ssh-keygen -N ''` would write. */
export async function makeKeyPair(comment) {
  const { sign, randomBytes } = await nacl();
  // TweetNaCl's secret key is seed ‖ public key, the 64 bytes OpenSSH stores.
  const pair = sign.keyPair();
  const type = bytes('ssh-ed25519');
  const blob = cat(str(type), str(pair.publicKey));
  const check = randomBytes(4);
  const secret = cat(check, check, str(type), str(pair.publicKey), str(pair.secretKey), str(bytes(comment)));
  // Padded to the "none" cipher's 8-byte blocks with 1, 2, 3, …
  const padded = cat(secret, Uint8Array.from({ length: (8 - (secret.length % 8)) % 8 }, (_, i) => i + 1));
  const file = cat(bytes('openssh-key-v1\0'), str(bytes('none')), str(bytes('none')), str(new Uint8Array()), u32(1), str(blob), str(padded));
  return {
    publicKey: `ssh-ed25519 ${b64(blob)} ${comment}`,
    privateKey: `-----BEGIN OPENSSH PRIVATE KEY-----\n${b64(file).match(/.{1,70}/g).join('\n')}\n-----END OPENSSH PRIVATE KEY-----\n`,
  };
}

/** Hand a file to the browser's downloads; the page keeps no copy. */
export function download(name, contents) {
  const url = URL.createObjectURL(new Blob([contents], { type: 'application/octet-stream' }));
  const a = Object.assign(document.createElement('a'), { href: url, download: name });
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10000);
}
