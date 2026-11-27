import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

class FakeClassList {
  values = new Set();

  add(...names) {
    for (const name of names) this.values.add(name);
  }

  remove(...names) {
    for (const name of names) this.values.delete(name);
  }
}

class FakeElement {
  className = '';
  classList = new FakeClassList();
  children = [];
  styleValues = new Map();
  style = {
    setProperty: (name, value) => {
      this.styleValues.set(name, value);
    },
  };
  textContent = '';

  append(...children) {
    this.children.push(...children);
  }

  replaceChildren(...children) {
    this.children = children;
  }
}

function createHarness(search = '') {
  const overlay = new FakeElement();
  const goals = new FakeElement();
  const documentElement = new FakeElement();
  let socket;

  class FakeWebSocket {
    listeners = new Map();

    constructor() {
      socket = this;
    }

    addEventListener(name, callback) {
      this.listeners.set(name, callback);
    }

    close() {}
  }

  const context = {
    document: {
      documentElement,
      createElement: () => new FakeElement(),
      getElementById: (id) => (id === 'overlay' ? overlay : goals),
    },
    Intl,
    Number,
    Set,
    URLSearchParams,
    WebSocket: FakeWebSocket,
    window: {
      clearTimeout() {},
      location: { host: '127.0.0.1:11180', search },
      setTimeout() {
        return 1;
      },
    },
  };

  const source = readFileSync(new URL('./app.js', import.meta.url), 'utf8');
  vm.runInNewContext(source, context, { filename: 'templates/goals/app.js' });

  return {
    goals,
    render(snapshot) {
      socket.listeners.get('message')({ data: JSON.stringify(snapshot) });
    },
  };
}

test('layout query selects horizontal, vertical, or 2x2 grid', () => {
  assert.ok(createHarness().goals.classList.values.has('layout-horizontal'));
  assert.ok(createHarness('?layout=vertical').goals.classList.values.has('layout-vertical'));
  assert.ok(createHarness('?layout=grid').goals.classList.values.has('layout-grid'));
  assert.ok(createHarness('?layout=invalid').goals.classList.values.has('layout-horizontal'));
});

test('skin query selects a built-in design and falls back to glass', () => {
  assert.ok(createHarness().goals.classList.values.has('skin-glass'));
  assert.ok(createHarness('?skin=solid').goals.classList.values.has('skin-solid'));
  assert.ok(createHarness('?skin=minimal').goals.classList.values.has('skin-minimal'));
  assert.ok(createHarness('?skin=invalid').goals.classList.values.has('skin-glass'));
});

test('custom PNG skin is applied as a card background image', () => {
  const harness = createHarness('?skin=custom&image=%2Fskin%2Fmy-goals.png');

  assert.ok(harness.goals.classList.values.has('skin-custom'));
  assert.equal(
    harness.goals.styleValues.get('--custom-skin-image'),
    'url("/skin/my-goals.png")',
  );
});

test('enabled goals render cards even when every target is zero', () => {
  const harness = createHarness();

  harness.render({
    comments: 0,
    viewers: 0,
    likes: 0,
    reactions: 0,
    donation: 0,
    commenters: 0,
    likesAvailable: true,
    reactionsAvailable: true,
    goalsEnabled: true,
    goalsVisible: {
      comments: true,
      viewers: true,
      likes: true,
      reactions: true,
      donation: true,
      commenters: true,
    },
    goals: {
      comments: 0,
      viewers: 0,
      likes: 0,
      reactions: 0,
      donation: 0,
      commenters: 0,
    },
  });

  assert.equal(harness.goals.children.length, 6);
  for (const card of harness.goals.children) {
    assert.equal(card.children[0].children[1].textContent, '0%');
    const zero = card.className.split(' ').includes('donation') ? '/ ¥0' : '/ 0';
    assert.equal(card.children[1].children[1].textContent, zero);
    assert.equal(card.children[2].children[0].style.width, '0%');
  }
});

test('disabled goals remain hidden when every target is zero', () => {
  const harness = createHarness();

  harness.render({
    comments: 0,
    viewers: 0,
    likes: 0,
    reactions: 0,
    donation: 0,
    commenters: 0,
    likesAvailable: true,
    reactionsAvailable: true,
    goalsEnabled: false,
    goalsVisible: {
      comments: true,
      viewers: true,
      likes: true,
      reactions: true,
      donation: true,
      commenters: true,
    },
    goals: {
      comments: 0,
      viewers: 0,
      likes: 0,
      reactions: 0,
      donation: 0,
      commenters: 0,
    },
  });

  assert.equal(harness.goals.children.length, 0);
});

test('individual goal visibility hides only unchecked metrics', () => {
  const harness = createHarness();

  harness.render({
    comments: 0,
    viewers: 0,
    likes: 0,
    reactions: 0,
    donation: 0,
    commenters: 0,
    likesAvailable: true,
    reactionsAvailable: true,
    goalsEnabled: true,
    goalsVisible: {
      comments: true,
      viewers: false,
      likes: true,
      reactions: false,
      donation: true,
      commenters: false,
    },
    goals: {
      comments: 0,
      viewers: 0,
      likes: 0,
      reactions: 0,
      donation: 0,
      commenters: 0,
    },
  });

  assert.equal(harness.goals.children.length, 3);
  assert.equal(harness.goals.children[0].children[0].children[0].textContent, 'LIKES');
  assert.equal(harness.goals.children[1].children[0].children[0].textContent, 'COMMENTS');
  assert.equal(harness.goals.children[2].children[0].children[0].textContent, 'DONATION');
});

function donationSnapshot() {
  return {
    comments: 12,
    viewers: 0,
    likes: 0,
    reactions: 0,
    donation: 1500,
    commenters: 7,
    likesAvailable: true,
    reactionsAvailable: true,
    goalsEnabled: true,
    goalsVisible: {
      comments: true,
      viewers: true,
      likes: true,
      reactions: true,
      donation: true,
      commenters: true,
    },
    goals: {
      comments: 100,
      viewers: 0,
      likes: 0,
      reactions: 0,
      donation: 3000,
      commenters: 10,
    },
  };
}

function cardByLabel(harness, label) {
  return harness.goals.children.find((card) => card.children[0].children[0].textContent === label);
}

test('donation shows yen sign on current and target values', () => {
  const harness = createHarness();
  harness.render(donationSnapshot());

  const card = cardByLabel(harness, 'DONATION');
  assert.ok(card);
  assert.ok(card.className.split(' ').includes('donation'));
  assert.equal(card.children[0].children[1].textContent, '50%');
  assert.equal(card.children[1].children[0].textContent, `¥${new Intl.NumberFormat().format(1500)}`);
  assert.equal(card.children[1].children[1].textContent, `/ ¥${new Intl.NumberFormat().format(3000)}`);
  assert.equal(card.children[2].children[0].style.width, '50%');
});

test('commenters shows plain count and progress', () => {
  const harness = createHarness();
  harness.render(donationSnapshot());

  const card = cardByLabel(harness, 'COMMENTERS');
  assert.ok(card);
  assert.ok(card.className.split(' ').includes('commenters'));
  assert.equal(card.children[0].children[1].textContent, '70%');
  assert.equal(card.children[1].children[0].textContent, '7');
  assert.equal(card.children[1].children[1].textContent, '/ 10');
});

test('only query filters to the new donation and commenters keys', () => {
  const harness = createHarness('?only=donation,COMMENTERS');
  harness.render(donationSnapshot());

  assert.deepEqual(
    harness.goals.children.map((card) => card.children[0].children[0].textContent),
    ['DONATION', 'COMMENTERS'],
  );
});

test('goals-neon / goals-retro use the same app.js as goals', () => {
  // 見た目は style.css だけで変える。app.js が分岐すると上のテストが変種を守れなくなる。
  const base = readFileSync(new URL('./app.js', import.meta.url), 'utf8');
  for (const name of ['goals-neon', 'goals-retro']) {
    const variant = readFileSync(new URL(`../${name}/app.js`, import.meta.url), 'utf8');
    assert.equal(variant, base, `${name}/app.js must match goals/app.js`);
  }
});
