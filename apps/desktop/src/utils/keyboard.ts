/** Controls that handle their own keys and clicks, such as a card's favorite button. */
const CONTROLS = 'a[href], button, input, select, textarea, [contenteditable]';

/** True when `target` is a control or inside one, so a shortcut leaves its keys alone. */
export function isInControl(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest(CONTROLS) !== null;
}

/** True while a text field has focus, so key shortcuts leave its keys alone. */
export function isEditingText(): boolean {
  const active = document.activeElement;
  if (!active) return false;
  const tag = active.tagName.toLowerCase();
  return tag === 'input' || tag === 'textarea' || (active as HTMLElement).isContentEditable;
}
