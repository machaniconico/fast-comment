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
    likesAvailable: true,
    reactionsAvailable: true,
    goalsEnabled: true,
    goalsVisible: {
      comments: true,
      viewers: true,
      likes: true,
      reactions: true,
    },
    goals: {
      comments: 0,
      viewers: 0,
      likes: 0,
      reactions: 0,
    },
  });

  assert.equal(harness.goals.children.length, 4);
  for (const card of harness.goals.children) {
    assert.equal(card.children[0].children[1].textContent, '0%');
    assert.equal(card.children[1].children[1].textContent, '/ 0');
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
    likesAvailable: true,
    reactionsAvailable: true,
    goalsEnabled: false,
    goalsVisible: {
      comments: true,
      viewers: true,
      likes: true,
      reactions: true,
    },
    goals: {
      comments: 0,
      viewers: 0,
      likes: 0,
      reactions: 0,
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
    likesAvailable: true,
    reactionsAvailable: true,
    goalsEnabled: true,
    goalsVisible: {
      comments: true,
      viewers: false,
      likes: true,
      reactions: false,
    },
    goals: {
      comments: 0,
      viewers: 0,
      likes: 0,
      reactions: 0,
    },
  });

  assert.equal(harness.goals.children.length, 2);
  assert.equal(harness.goals.children[0].children[0].children[0].textContent, 'LIKES');
  assert.equal(harness.goals.children[1].children[0].children[0].textContent, 'COMMENTS');
});
