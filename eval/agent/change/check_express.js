// Hidden acceptance tests. Lives outside the agent's repo copy.
const path = require('path'), fs = require('fs'), os = require('os');
const assert = require('assert');
const root = process.argv[2], task = process.argv[3];
const express = require(path.join(root, 'index.js'));

async function hit(app, url = '/', opts = {}) {
  const s = await new Promise(r => { const x = app.listen(0, () => r(x)); });
  try {
    const res = await fetch(`http://127.0.0.1:${s.address().port}${url}`, { redirect: 'manual', ...opts });
    return { status: res.status, h: res.headers, body: await res.text() };
  } finally { s.close(); s.closeAllConnections && s.closeAllConnections(); }
}
const mk = (fn) => { const a = express(); a.get('/', fn); return a; };

const T = {
  async E1() {
    const r = await hit(mk((req, res) => res.noStore().send('ok')));
    assert.equal(r.status, 200); assert.equal(r.h.get('cache-control'), 'no-store'); assert.equal(r.body, 'ok');
  },
  async E2() {
    const a = mk((req, res) => res.json({ t: req.bearerToken === undefined ? 'UNDEF' : req.bearerToken }));
    const v = async (auth) => JSON.parse((await hit(a, '/', auth ? { headers: { authorization: auth } } : {})).body).t;
    assert.equal(await v('Bearer abc'), 'abc');
    assert.equal(await v('bearer xyz'), 'xyz');
    assert.equal(await v(null), 'UNDEF');
    assert.equal(await v('Basic zzz'), 'UNDEF');
  },
  async E3() {
    const obj = { b: 1, a: { d: 1, c: [{ z: 1, y: 2 }] } };
    const on = mk((req, res) => res.json(obj)); on.set('json sort keys', true);
    assert.equal((await hit(on)).body, '{"a":{"c":[{"y":2,"z":1}],"d":1},"b":1}');
    assert.equal((await hit(mk((req, res) => res.json(obj)))).body, JSON.stringify(obj));
  },
  async E4() {
    const f = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'dl-')), 'f.txt'); fs.writeFileSync(f, 'hi');
    const a = await hit(mk((req, res) => res.download(f, 'x.txt', { inline: true })));
    assert.equal(a.status, 200); assert.match(a.h.get('content-disposition'), /^inline/); assert.match(a.h.get('content-disposition'), /x\.txt/);
    const b = await hit(mk((req, res) => res.download(f, 'x.txt')));
    assert.match(b.h.get('content-disposition'), /^attachment/);
  },
  async E5() {
    const t = async (fn) => (await hit(mk((req, res) => {
      try { fn(res); } catch (e) { res.status(599).send(e.name); }
    })));
    for (const bad of [(r) => r.redirect(123), (r) => r.redirect(''), (r) => r.redirect(undefined)]) {
      const r = await t(bad); assert.equal(r.status, 599); assert.equal(r.body, 'TypeError');
    }
    const ok = await t((r) => r.redirect(301, '/x')); assert.equal(ok.status, 301);
  },
  async E6() {
    const mkApp = (fn) => { const a = mk(fn); a.set('default cookie options', { httpOnly: true, sameSite: 'lax' }); return a; };
    let c = (await hit(mkApp((req, res) => res.cookie('a', 'b').send('x')))).h.get('set-cookie');
    assert.match(c, /HttpOnly/); assert.match(c, /SameSite=Lax/);
    c = (await hit(mkApp((req, res) => res.cookie('a', 'b', { httpOnly: false }).send('x')))).h.get('set-cookie');
    assert.doesNotMatch(c, /HttpOnly/); assert.match(c, /SameSite=Lax/);
    c = (await hit(mkApp((req, res) => res.clearCookie('a').send('x')))).h.get('set-cookie');
    assert.match(c, /SameSite=Lax/);
    c = (await hit(mk((req, res) => res.cookie('a', 'b').send('x')))).h.get('set-cookie');
    assert.doesNotMatch(c, /HttpOnly/);
  },
  async E7() {
    const a = mk((req, res) => res.send('hello'));
    const first = await hit(a); assert.equal(first.status, 200);
    const tag = first.h.get('etag'); assert.ok(tag);
    // fetch adds Cache-Control: no-cache to conditional requests, which disables 304.
    const http = require('http');
    const s = await new Promise(r => { const x = a.listen(0, () => r(x)); });
    const status = await new Promise((r, j) => {
      http.get({ port: s.address().port, path: '/', headers: { 'if-none-match': tag } }, (res) => { res.resume(); r(res.statusCode); }).on('error', j);
    });
    s.close(); s.closeAllConnections && s.closeAllConnections();
    assert.equal(status, 304);
  },
};

T[task]().then(() => { console.log('PASS'); process.exit(0); },
  (e) => { console.log('FAIL ' + String(e.message).split('\n')[0]); process.exit(1); });
