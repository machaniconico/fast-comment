/** 設定の絞り込み用に、全角・半角と大文字・小文字の違いをならす。 */
export function normalizeSettingsText(text: string): string {
  return text.normalize('NFKC').toLowerCase();
}

/** 空白で区切った語がすべて text に含まれれば true。query が空なら常に true。 */
export function matchesSettingsQuery(text: string, query: string): boolean {
  const terms = normalizeSettingsText(query).split(/\s+/).filter((t) => t !== '');
  if (terms.length === 0) return true;
  const haystack = normalizeSettingsText(text);
  return terms.every((t) => haystack.includes(t));
}
