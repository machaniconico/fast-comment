/**
 * 設定画面の節の開閉・絞り込みの状態(シングルトン)。
 * 開閉の記憶は localStorage の `fc.settingsOpen` に置く。絞り込みで一時的に開いた分は記憶しない。
 */
import { parseOpenIds, serializeOpenIds, SETTINGS_OPEN_KEY, SETTINGS_SECTION_IDS } from './settingsOpen';

function load(): string[] {
  try {
    return parseOpenIds(localStorage.getItem(SETTINGS_OPEN_KEY));
  } catch {
    return [];
  }
}

function save(ids: string[]): void {
  try {
    localStorage.setItem(SETTINGS_OPEN_KEY, serializeOpenIds(ids));
  } catch {
    // 保存できなくても、その回の開閉は動く。
  }
}

class SettingsCollapse {
  /** 折り畳みを効かせるか(focus === 'all' の時だけ true)。 */
  enabled = $state(false);
  query = $state('');
  openIds = $state<string[]>(load());
  /** 絞り込みに一致している節(描画中のもの)。 */
  matches = $state<Record<string, boolean>>({});

  isOpen(id: string): boolean {
    return this.openIds.includes(id);
  }

  toggle(id: string): void {
    this.openIds = this.isOpen(id) ? this.openIds.filter((v) => v !== id) : [...this.openIds, id];
    save(this.openIds);
  }

  open(id: string): void {
    if (this.isOpen(id) || !SETTINGS_SECTION_IDS.includes(id)) return;
    this.openIds = [...this.openIds, id];
    save(this.openIds);
  }

  openAll(): void {
    this.openIds = [...SETTINGS_SECTION_IDS];
    save(this.openIds);
  }

  closeAll(): void {
    this.openIds = [];
    save(this.openIds);
  }

  get noMatch(): boolean {
    // 目標・弾幕の節は focus='all' では CSS で隠れているので数えない。
    const visible = Object.entries(this.matches).filter(([id]) => id !== 'settings-goals' && id !== 'settings-danmaku');
    return this.query.trim() !== '' && !visible.some(([, m]) => m);
  }
}

export const settingsCollapse = new SettingsCollapse();
