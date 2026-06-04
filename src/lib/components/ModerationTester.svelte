<script lang="ts">
  interface Props {
    ngWords: string[];
    ngUsers: string[];
    highlights: string[];
  }

  type RuleKind = 'ngUser' | 'ngWord' | 'highlight';
  type MatchTarget = '名前' | 'ID' | '本文';
  type Verdict = 'empty' | 'ngUser' | 'ngWord' | 'highlight' | 'show';

  interface CompiledRule {
    kind: RuleKind;
    pattern: string;
    regex: RegExp;
  }

  interface InvalidRule {
    kind: RuleKind;
    pattern: string;
    message: string;
  }

  interface RuleMatch {
    kind: RuleKind;
    pattern: string;
    target: MatchTarget;
    value: string;
  }

  const KIND_LABELS: Record<RuleKind, string> = {
    ngUser: 'NGユーザー',
    ngWord: 'NGワード',
    highlight: 'ハイライト'
  };

  let { ngWords = [], ngUsers = [], highlights = [] }: Props = $props();

  let testText: string = $state('');
  let authorName: string = $state('');
  let authorId: string = $state('');

  const compiled = $derived.by(() => {
    const valid: CompiledRule[] = [];
    const invalid: InvalidRule[] = [];

    compileRules(ngUsers, 'ngUser', valid, invalid);
    compileRules(ngWords, 'ngWord', valid, invalid);
    compileRules(highlights, 'highlight', valid, invalid);

    return { valid, invalid };
  });

  const preview = $derived.by(() => {
    const text = testText;
    const name = authorName.trim();
    const id = authorId.trim();
    const hasInput = text.length > 0 || name.length > 0 || id.length > 0;

    if (!hasInput) {
      return {
        verdict: 'empty' as Verdict,
        matches: [] as RuleMatch[],
        effectiveMatches: [] as RuleMatch[]
      };
    }

    const ngUserMatches = findMatches(compiled.valid, 'ngUser', [
      ['名前', name],
      ['ID', id]
    ]);
    if (ngUserMatches.length > 0) {
      return {
        verdict: 'ngUser' as Verdict,
        matches: ngUserMatches,
        effectiveMatches: ngUserMatches
      };
    }

    const ngWordMatches = findMatches(compiled.valid, 'ngWord', [['本文', text]]);
    if (ngWordMatches.length > 0) {
      return {
        verdict: 'ngWord' as Verdict,
        matches: ngWordMatches,
        effectiveMatches: ngWordMatches
      };
    }

    const highlightMatches = findMatches(compiled.valid, 'highlight', [
      ['本文', text],
      ['名前', name]
    ]);
    if (highlightMatches.length > 0) {
      return {
        verdict: 'highlight' as Verdict,
        matches: highlightMatches,
        effectiveMatches: highlightMatches
      };
    }

    return {
      verdict: 'show' as Verdict,
      matches: [] as RuleMatch[],
      effectiveMatches: [] as RuleMatch[]
    };
  });

  const verdictLabel = $derived.by(() => {
    switch (preview.verdict) {
      case 'ngUser':
        return 'Hide(NGユーザー)';
      case 'ngWord':
        return 'Hide(NGワード)';
      case 'highlight':
        return 'Highlight';
      case 'show':
        return '表示';
      default:
        return '未入力';
    }
  });

  function compileRules(
    patterns: string[],
    kind: RuleKind,
    valid: CompiledRule[],
    invalid: InvalidRule[]
  ) {
    for (const pattern of patterns) {
      try {
        valid.push({ kind, pattern, regex: new RegExp(pattern) });
      } catch (e) {
        invalid.push({
          kind,
          pattern,
          message: e instanceof Error ? e.message : String(e)
        });
      }
    }
  }

  function findMatches(
    rules: CompiledRule[],
    kind: RuleKind,
    targets: Array<[MatchTarget, string]>
  ): RuleMatch[] {
    const matches: RuleMatch[] = [];
    for (const rule of rules) {
      if (rule.kind !== kind) continue;
      for (const [target, value] of targets) {
        if (!value) continue;
        if (rule.regex.test(value)) {
          matches.push({ kind, pattern: rule.pattern, target, value });
        }
      }
    }
    return matches;
  }
</script>

<section class="moderation-tester" aria-labelledby="moderation-tester-title">
  <div class="tester-header">
    <div>
      <h4 id="moderation-tester-title">NG/ハイライト テスト</h4>
      <p class="tester-note">
        本文は1つの text fragment として扱います。JS RegExp 近似のため Rust regex と完全互換ではありません。
      </p>
    </div>
    <span class:hide={preview.verdict === 'ngUser' || preview.verdict === 'ngWord'} class:highlight={preview.verdict === 'highlight'} class:show={preview.verdict === 'show'} class:empty={preview.verdict === 'empty'} class="verdict-badge">
      {verdictLabel}
    </span>
  </div>

  <div class="tester-grid">
    <label class="tester-field" for="moderation-test-text">
      <span>テスト本文</span>
      <textarea
        id="moderation-test-text"
        bind:value={testText}
        rows={3}
        aria-label="モデレーション判定をテストする本文"
        placeholder="判定したいコメント本文"
      ></textarea>
    </label>

    <label class="tester-field" for="moderation-test-author">
      <span>投稿者名（任意）</span>
      <input
        id="moderation-test-author"
        type="text"
        bind:value={authorName}
        aria-label="モデレーション判定をテストする投稿者名"
        placeholder="username"
      />
    </label>

    <label class="tester-field" for="moderation-test-author-id">
      <span>投稿者ID（任意）</span>
      <input
        id="moderation-test-author-id"
        type="text"
        bind:value={authorId}
        aria-label="モデレーション判定をテストする投稿者ID"
        placeholder="channel-id"
      />
    </label>
  </div>

  <div class="tester-result" aria-live="polite">
    {#if preview.verdict === 'empty'}
      <p class="muted">本文または投稿者を入力すると、現在の規則で近似判定します。</p>
    {:else if preview.effectiveMatches.length > 0}
      <p class="result-title">マッチした規則</p>
      <ul>
        {#each preview.effectiveMatches as match}
          <li>
            <span class="rule-kind">{KIND_LABELS[match.kind]}</span>
            <code>{match.pattern}</code>
            <span class="muted">→ {match.target}</span>
          </li>
        {/each}
      </ul>
    {:else}
      <p class="muted">マッチした規則はありません。</p>
    {/if}

    {#if compiled.invalid.length > 0}
      <p class="result-title invalid-title">無効な規則（スキップ）</p>
      <ul>
        {#each compiled.invalid as invalid}
          <li>
            <span class="rule-kind">{KIND_LABELS[invalid.kind]}</span>
            <code>{invalid.pattern}</code>
            <span class="muted">正規表現エラー</span>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
</section>

<style>
  .moderation-tester {
    margin-top: 14px;
    padding: 10px;
    border: 1px solid rgba(255,255,255,0.1);
    border-radius: 6px;
    background: rgba(255,255,255,0.035);
  }

  .tester-header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 10px;
  }

  h4 {
    margin: 0;
    color: #e0e0e0;
    font-size: 13px;
    font-weight: 700;
  }

  .tester-note {
    margin: 3px 0 0;
    color: #858585;
    font-size: 11px;
    line-height: 1.4;
  }

  .verdict-badge {
    flex: 0 0 auto;
    border-radius: 999px;
    padding: 4px 9px;
    color: #e0e0e0;
    background: #455a64;
    font-size: 12px;
    font-weight: 700;
    line-height: 1.2;
    white-space: nowrap;
  }

  .verdict-badge.hide {
    background: #6d2f2f;
    color: #ffcdd2;
  }

  .verdict-badge.highlight {
    background: #6d5515;
    color: #ffe082;
  }

  .verdict-badge.show {
    background: #1f5b35;
    color: #b9f6ca;
  }

  .verdict-badge.empty {
    background: #455a64;
    color: #cfd8dc;
  }

  .tester-grid {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(120px, 180px) minmax(120px, 180px);
    gap: 8px;
    margin-top: 10px;
  }

  .tester-field {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 4px;
    color: #ccc;
    font-size: 12px;
    font-weight: 600;
  }

  .tester-field input,
  .tester-field textarea {
    width: 100%;
    min-width: 0;
    box-sizing: border-box;
    border: 1px solid rgba(255,255,255,0.12);
    border-radius: 4px;
    background: rgba(255,255,255,0.07);
    color: #e0e0e0;
    font-size: 12px;
    padding: 6px 8px;
  }

  .tester-field textarea {
    resize: vertical;
    font-family: inherit;
  }

  .tester-result {
    margin-top: 10px;
    color: #d0d0d0;
    font-size: 12px;
  }

  .tester-result p {
    margin: 0;
  }

  .result-title {
    color: #bdbdbd;
    font-weight: 700;
  }

  .invalid-title {
    margin-top: 8px;
    color: #ffca28;
  }

  ul {
    margin: 5px 0 0;
    padding: 0;
    list-style: none;
  }

  li {
    display: flex;
    align-items: baseline;
    gap: 6px;
    min-width: 0;
    margin-top: 4px;
    flex-wrap: wrap;
  }

  code {
    min-width: 0;
    color: #e0e0e0;
    font-family: monospace;
    overflow-wrap: anywhere;
  }

  .rule-kind {
    color: #90caf9;
    font-weight: 700;
  }

  .muted {
    color: #858585;
  }

  @media (max-width: 860px) {
    .tester-grid {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
