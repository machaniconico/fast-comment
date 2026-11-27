// 第三者ライセンス表記 THIRD_PARTY_NOTICES.txt の生成スクリプト。
//
// 対象は「配布物に実際に入る依存」だけ:
//   - Rust: Windows ターゲット(x86_64-pc-windows-msvc)の normal 依存。dev 依存は除外。
//           build 依存(build.rs 専用)は成果物に入らないので除外。
//           proc-macro は normal 依存として解決されるため、保守的に含める。
//   - npm : package.json の dependencies を起点に、実行時依存(dependencies)を再帰的にたどったもの。
//           devDependencies(vite/svelte 等)は含めない。
// 実行: pnpm licenses  (cargo と、node_modules / ~/.cargo/registry に依存の取得済みソースが必要)
import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const tauriDir = path.join(root, 'src-tauri');
const textsDir = path.join(path.dirname(fileURLToPath(import.meta.url)), 'texts');
const outFile = path.join(root, 'THIRD_PARTY_NOTICES.txt');
const WINDOWS_TARGET = 'x86_64-pc-windows-msvc';
const COPYLEFT = /\b(A?GPL|LGPL|MPL|EPL|CDDL|OSL|EUPL|CPL|SSPL)/i;
const LICENSE_FILE = /^(licen[sc]e|copying|unlicense|copyright|notice)/i;

/** @typedef {{ ecosystem: 'Rust' | 'npm', name: string, version: string, license: string, dir: string }} Pkg */

function readText(file) {
  return readFileSync(file, 'utf8').replace(/^﻿/, '').replace(/\r\n?/g, '\n').trim();
}

/** パッケージ直下のライセンス / NOTICE ファイルを集める。 */
function collectTexts(dir, extraFile) {
  const names = new Set();
  for (const entry of readdirSync(dir)) {
    const full = path.join(dir, entry);
    if (LICENSE_FILE.test(entry) && statSync(full).isFile()) names.add(entry);
  }
  if (extraFile && existsSync(path.join(dir, extraFile))) names.add(extraFile);
  return [...names]
    .sort()
    .map((name) => ({ name, text: readText(path.join(dir, name)) }))
    .filter((item) => item.text.length > 0);
}

/** @returns {Pkg[]} */
function rustPackages() {
  const json = execFileSync(
    'cargo',
    ['metadata', '--format-version', '1', '--filter-platform', WINDOWS_TARGET],
    { cwd: tauriDir, maxBuffer: 1 << 28, encoding: 'utf8' },
  );
  const meta = JSON.parse(json);
  const byId = new Map(meta.packages.map((p) => [p.id, p]));
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const seen = new Set();
  const stack = [meta.resolve.root];
  while (stack.length > 0) {
    const id = stack.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    for (const dep of nodes.get(id).deps) {
      // kind が null のもの = normal 依存。dev / build だけの辺はたどらない。
      if (dep.dep_kinds.some((k) => k.kind === null)) stack.push(dep.pkg);
    }
  }
  seen.delete(meta.resolve.root);
  return [...seen].map((id) => {
    const p = byId.get(id);
    return {
      ecosystem: 'Rust',
      name: p.name,
      version: p.version,
      license: p.license ?? (p.license_file ? `(license-file: ${p.license_file})` : ''),
      dir: path.dirname(p.manifest_path),
      extraFile: p.license_file ?? undefined,
      authors: p.authors ?? [],
      repository: p.repository ?? '',
    };
  });
}

/** node 流の解決(realpath から親の node_modules を順にたどる。pnpm の .pnpm 配下にも対応)。 */
function resolveNpm(name, fromDir) {
  let dir = fromDir;
  for (;;) {
    const candidate = path.join(dir, 'node_modules', name);
    if (existsSync(path.join(candidate, 'package.json'))) return realpathSync(candidate);
    const parent = path.dirname(dir);
    if (parent === dir) return null;
    dir = parent;
  }
}

/** @returns {Pkg[]} */
function npmPackages() {
  const rootPkg = JSON.parse(readFileSync(path.join(root, 'package.json'), 'utf8'));
  const found = new Map();
  const queue = Object.keys(rootPkg.dependencies ?? {}).map((name) => ({ name, from: root }));
  while (queue.length > 0) {
    const { name, from } = queue.pop();
    const dir = resolveNpm(name, from);
    if (!dir) throw new Error(`npm 依存 ${name} が見つかりません(pnpm install 済みか確認)`);
    if (found.has(dir)) continue;
    const pkg = JSON.parse(readFileSync(path.join(dir, 'package.json'), 'utf8'));
    const license =
      typeof pkg.license === 'string'
        ? pkg.license
        : (pkg.license?.type ?? (pkg.licenses ?? []).map((l) => l.type).join(' OR '));
    found.set(dir, { ecosystem: 'npm', name: pkg.name, version: pkg.version, license, dir });
    for (const dep of Object.keys(pkg.dependencies ?? {})) queue.push({ name: dep, from: dir });
  }
  return [...found.values()];
}

/** コピーレフト判定。OR の選択肢に許容ライセンスがあれば「選択可能」、無ければ「強制」。 */
function copyleftKind(license) {
  if (!COPYLEFT.test(license)) return null;
  const alternatives = license.replace(/[()]/g, '').split(/\s+OR\s+|\s*\/\s*/i);
  return alternatives.every((alt) => COPYLEFT.test(alt)) ? 'forced' : 'optional';
}

const packages = [...rustPackages(), ...npmPackages()].sort(
  (a, b) => a.ecosystem.localeCompare(b.ecosystem) || a.name.localeCompare(b.name) || a.version.localeCompare(b.version),
);

/**
 * LICENSE ファイルが配布物に無いパッケージ向けに、宣言された SPDX 種別の標準全文(texts/<SPDX>.txt。
 * 他 crate に同梱の全文からコピーしたもの)を補う。MIT / BSD は Cargo.toml の authors から著作権表示を作る。
 */
function supplementText(pkg) {
  const file = path.join(textsDir, `${pkg.license}.txt`);
  if (!pkg.license || !existsSync(file)) return null;
  const body = readText(file);
  if (pkg.license !== 'MIT' && pkg.license !== 'BSD-3-Clause') return body;
  const owner = pkg.authors?.length ? pkg.authors.join(', ') : `${pkg.name} の作者`;
  return `Copyright (c) ${owner}\n\n${body}`;
}

const groups = new Map();
const supplemented = [];
const mpl = []; // 本文 -> { texts の由来ファイル名, パッケージ }
const missing = [];
const copyleft = [];
for (const pkg of packages) {
  const label = `${pkg.name} ${pkg.version} (${pkg.license || 'ライセンス種別不明'})`;
  const kind = copyleftKind(pkg.license);
  if (kind) copyleft.push({ label, kind });
  if (pkg.license === 'MPL-2.0' && pkg.ecosystem === 'Rust') mpl.push(pkg);
  let texts = collectTexts(pkg.dir, pkg.extraFile);
  let note = '';
  if (texts.length === 0) {
    const text = supplementText(pkg);
    if (text) {
      texts = [{ name: `(標準全文を補足: ${pkg.license})`, text }];
      note = ` ※配布元にライセンス文が無いため、宣言された種別の標準全文を補いました。repository: ${pkg.repository || '(未記載)'}`;
      supplemented.push({ label, pkg });
    }
  }
  if (texts.length === 0 || !pkg.license) missing.push({ label, pkg, hasText: texts.length > 0 });
  for (const { name, text } of texts) {
    if (!groups.has(text)) groups.set(text, { files: new Set(), labels: [] });
    const group = groups.get(text);
    group.files.add(name);
    group.labels.push(`[${pkg.ecosystem}] ${label}${note}`);
  }
}

const count = (eco) => packages.filter((p) => p.ecosystem === eco).length;
const licenseCounts = new Map();
for (const p of packages) licenseCounts.set(p.license || '(不明)', (licenseCounts.get(p.license || '(不明)') ?? 0) + 1);

const bar = '='.repeat(78);
const lines = [];
lines.push('fast-comment 第三者ライセンス表記 (THIRD PARTY NOTICES)');
lines.push('');
lines.push('このファイルは fast-comment に同梱される第三者ソフトウェアのライセンス表記です。');
lines.push('scripts/licenses/generate.mjs が自動生成します。手で編集しないでください。');
lines.push(`対象: Rust ${count('Rust')} 件(Windows ターゲットの実行時依存)、npm ${count('npm')} 件(本番 dependencies とその実行時依存)`);
lines.push('LICENSE ファイルが配布元に無いパッケージは、宣言された種別の標準全文を補い、その旨を各項目に明記しています。');
lines.push('同一の本文は 1 回にまとめ、その本文を持つパッケージを列挙しています。');
lines.push('各パッケージの著作権表示は、本文に含まれるものをそのまま掲載しています。');
lines.push('');
if (mpl.length > 0) {
  lines.push(bar);
  lines.push('MPL-2.0 のコンポーネントのソースコード入手先');
  lines.push(bar);
  lines.push('以下の MPL-2.0 のコンポーネントは改変せずに使用しています。ソースコードは次の場所から入手できます。');
  lines.push('');
  for (const p of mpl) {
    lines.push(`  - ${p.name} ${p.version}`);
    lines.push(`      https://crates.io/crates/${p.name}/${p.version}`);
    if (p.repository) lines.push(`      ${p.repository}`);
  }
  lines.push('');
}
if (missing.length > 0) {
  lines.push(bar);
  lines.push('ライセンス文が見つからなかったパッケージ');
  lines.push(bar);
  lines.push('次のパッケージは、配布物内にライセンス文(LICENSE 等)または種別の宣言が見つかりませんでした。');
  lines.push('宣言されたライセンス種別のみ記載します。全文は各パッケージの配布元を参照してください。');
  lines.push('');
  for (const m of missing) lines.push(`  - [${m.pkg.ecosystem}] ${m.label}${m.hasText ? ' ※ライセンス種別の宣言なし' : ''}`);
  lines.push('');
}
for (const [text, group] of [...groups].sort((a, b) => a[1].labels[0].localeCompare(b[1].labels[0]))) {
  lines.push(bar);
  lines.push(`ファイル: ${[...group.files].join(', ')}`);
  lines.push('対象パッケージ:');
  for (const label of group.labels) lines.push(`  - ${label}`);
  lines.push(bar);
  lines.push(text);
  lines.push('');
}
writeFileSync(outFile, '﻿' + lines.join('\n') + '\n', 'utf8');

const err = (s) => process.stderr.write(s + '\n');
err(`THIRD_PARTY_NOTICES.txt: Rust ${count('Rust')} 件 / npm ${count('npm')} 件、本文 ${groups.size} 種、${statSync(outFile).size} バイト`);
err('ライセンス種別の内訳:');
for (const [license, n] of [...licenseCounts].sort((a, b) => b[1] - a[1])) err(`  ${n}\t${license}`);
if (supplemented.length > 0) {
  err(`情報: 標準全文を補ったパッケージ ${supplemented.length} 件`);
  for (const m of supplemented) err(`  - ${m.label}`);
}
if (missing.length > 0) {
  err(`警告: ライセンス文または種別が見つからないパッケージ ${missing.length} 件`);
  for (const m of missing) err(`  - ${m.label}${m.hasText ? ' (種別の宣言なし)' : ''}`);
}
if (copyleft.length > 0) {
  err(`警告: コピーレフト系ライセンスを含むパッケージ ${copyleft.length} 件`);
  for (const c of copyleft) err(`  - ${c.label} [${c.kind === 'forced' ? '選択肢なし' : 'OR で許容ライセンスを選択可能'}]`);
}
