/**
 * Content script (every frame): types the live transcript into the text box the
 * user locked from the side panel.
 *
 * The in-progress (partial) text is inserted as it arrives and corrected in place
 * when Deepgram revises it: only the characters that differ are replaced, so the
 * box stays in sync with the side panel. A final segment replaces the partial and
 * is committed; the next segment starts after it.
 *
 * Loaded as a classic script, so it must not import or export anything.
 */
(() => {
  type InserterMessage =
    | { type: "sac-sync"; text: string; final: boolean }
    | { type: "sac-commit"; text: string }
    | { type: "sac-retract"; count: number }
    | { type: "sac-reset" };
  interface Inserter {
    probe(): { editable: boolean; focusedAt: number };
    alive(): boolean;
  }

  const scope = globalThis as { __sacInserter?: Inserter };
  if (scope.__sacInserter?.alive()) return;

  const TEXT_INPUT_TYPES = new Set(["text", "search", "url", "tel"]);

  /** Google Docs receives keyboard input in a hidden iframe instead of the visible page. */
  const isGoogleDocs = (() => {
    try {
      return window.frameElement?.classList.contains("docs-texteventtarget-iframe") ?? false;
    } catch {
      return false;
    }
  })();

  let lastEditable: HTMLElement | null = null;
  let lastEditableId = "";
  let focusedAt = 0;
  let savedRange: Range | null = null;
  /** Text this script inserted for the current, not yet final, segment (including its leading space). */
  let live = "";
  let prefix = "";
  let wroteFinal = false;

  function isTextField(node: unknown): node is HTMLInputElement | HTMLTextAreaElement {
    return (
      node instanceof HTMLTextAreaElement ||
      (node instanceof HTMLInputElement && TEXT_INPUT_TYPES.has(node.type))
    );
  }

  function isEditable(node: unknown): node is HTMLElement {
    return isTextField(node) || (node instanceof HTMLElement && node.isContentEditable);
  }

  function deepActiveElement(): Element | null {
    let active = document.activeElement;
    while (active?.shadowRoot?.activeElement) active = active.shadowRoot.activeElement;
    return active;
  }

  function resetSegment(): void {
    live = "";
    prefix = "";
  }

  document.addEventListener(
    "focusin",
    (event) => {
      const node = event.composedPath()[0];
      if (!isEditable(node) && !isGoogleDocs) return;
      lastEditable = isEditable(node) ? node : null;
      lastEditableId = lastEditable?.id ?? "";
      focusedAt = Date.now();
    },
    true,
  );

  document.addEventListener("selectionchange", () => {
    if (!lastEditable?.isContentEditable) return;
    const selection = getSelection();
    if (selection?.rangeCount && lastEditable.contains(selection.anchorNode)) {
      savedRange = selection.getRangeAt(0).cloneRange();
    }
  });

  /**
   * The focused text box, or the last one focused if focus has since moved elsewhere
   * on the page. Editors like ChatGPT's replace their element when the layout changes,
   * so a detached box is looked up again by id.
   */
  function targetElement(): HTMLElement | null {
    if (isGoogleDocs) return (document.activeElement as HTMLElement | null) ?? document.body;
    const active = deepActiveElement();
    if (isEditable(active)) return active;
    if (lastEditable?.isConnected) return lastEditable;
    const byId = lastEditableId ? document.getElementById(lastEditableId) : null;
    return isEditable(byId) ? byId : null;
  }

  function caretInside(el: HTMLElement, selection: Selection): boolean {
    return selection.rangeCount > 0 && el.contains(selection.anchorNode) && el.contains(selection.focusNode);
  }

  /** Puts the caret after the last text in a rich-text box. */
  function placeCaretAtEnd(el: HTMLElement, selection: Selection): void {
    const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    let lastText: Text | null = null;
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      if (node.textContent) lastText = node as Text;
    }
    const range = document.createRange();
    if (lastText) {
      range.setStart(lastText, lastText.length);
    } else {
      let container: Node = el;
      while (container.lastChild instanceof HTMLElement && !(container.lastChild instanceof HTMLBRElement)) {
        container = container.lastChild;
      }
      range.selectNodeContents(container);
      range.collapse(false);
    }
    range.collapse(true);
    selection.removeAllRanges();
    selection.addRange(range);
  }

  /**
   * Focus the box (without activating the tab) and leave a collapsed caret inside it.
   * Never lets the caret stay outside the box, or text would land elsewhere on the page.
   */
  function prepareCaret(el: HTMLElement): void {
    const selection = getSelection();
    if (deepActiveElement() !== el) {
      el.focus({ preventScroll: true });
      if (el.isContentEditable && selection) {
        // focus() puts the caret at the start of a rich-text box; restore it or go to the end.
        if (savedRange && el.contains(savedRange.startContainer)) {
          selection.removeAllRanges();
          selection.addRange(savedRange);
        } else {
          placeCaretAtEnd(el, selection);
        }
      }
    }
    if (isTextField(el)) {
      const end = el.selectionEnd ?? el.value.length;
      if (el.selectionStart !== end) el.setSelectionRange(end, end);
      return;
    }
    if (!selection) return;
    if (!caretInside(el, selection)) placeCaretAtEnd(el, selection);
    else if (!selection.isCollapsed) selection.collapseToEnd();
  }

  /** Text before the caret, or null when it can't be read. */
  function textBeforeCaret(el: HTMLElement): string | null {
    if (isTextField(el)) {
      const start = el.selectionStart;
      return start === null ? null : el.value.slice(0, start);
    }
    const selection = getSelection();
    if (!selection?.rangeCount) return null;
    const caret = selection.getRangeAt(0);
    if (!el.contains(caret.startContainer)) return null;
    const before = document.createRange();
    before.selectNodeContents(el);
    before.setEnd(caret.startContainer, caret.startOffset);
    return before.toString().replace(/\u00a0/g, " ");
  }

  function execInsert(text: string): boolean {
    return text ? document.execCommand("insertText", false, text) : document.execCommand("delete");
  }

  function dispatchInput(el: HTMLElement, text: string): void {
    el.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: text }));
  }

  /** Replace the `remove` characters before the caret with `add`. */
  function replaceBeforeCaret(el: HTMLElement, remove: number, add: string): void {
    if (isGoogleDocs) {
      for (let i = 0; i < remove; i++) {
        el.dispatchEvent(
          new KeyboardEvent("keydown", {
            key: "Backspace",
            code: "Backspace",
            keyCode: 8,
            which: 8,
            bubbles: true,
            cancelable: true,
          }),
        );
      }
      if (add) {
        const data = new DataTransfer();
        data.setData("text/plain", add);
        el.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true, cancelable: true }));
      }
      return;
    }

    if (isTextField(el)) {
      const end = el.selectionEnd ?? el.value.length;
      if (remove) el.setSelectionRange(end - remove, end);
      if (!execInsert(add)) {
        el.setRangeText(add, end - remove, end, "end");
        dispatchInput(el, add);
      }
      return;
    }

    const selection = getSelection();
    if (!selection) return;
    for (let i = 0; i < remove; i++) selection.modify("extend", "backward", "character");
    if (!execInsert(add) && selection.rangeCount) {
      const range = selection.getRangeAt(0);
      range.deleteContents();
      if (add) {
        const node = document.createTextNode(add);
        range.insertNode(node);
        range.setStartAfter(node);
        range.collapse(true);
        selection.removeAllRanges();
        selection.addRange(range);
      }
      dispatchInput(el, add);
    }
  }

  function sync(text: string, final: boolean): boolean {
    const el = targetElement();
    if (!el) return false;
    if (!isGoogleDocs) prepareCaret(el);

    // Google Docs gives no way to read the text, so the previous insert is trusted there.
    const before = isGoogleDocs ? null : textBeforeCaret(el);
    // Unless the text before the caret still ends with what was inserted (the user typed,
    // moved the caret, switched boxes, or the editor re-rendered without it), never delete:
    // start the segment over from the caret and leave the existing text alone.
    if (live && !isGoogleDocs && !(before ?? "").endsWith(live)) resetSegment();

    if (!live && text) {
      const needsSpace = before === null ? wroteFinal : before.length > 0 && !/\s$/.test(before);
      prefix = needsSpace ? " " : "";
    }
    const next = text ? prefix + text : "";

    let common = 0;
    while (common < live.length && common < next.length && live[common] === next[common]) common++;
    const remove = live.length - common;
    const add = next.slice(common);
    if (remove || add) replaceBeforeCaret(el, remove, add);

    live = next;
    if (final || !text) {
      if (final && next) wroteFinal = true;
      resetSegment();
    }
    return true;
  }

  /** Insert catch-up text verbatim; spacing is already decided by the background. */
  function commit(text: string): boolean {
    if (!text) return true;
    const el = targetElement();
    if (!el) return false;
    if (!isGoogleDocs) prepareCaret(el);
    if (live) {
      // Drop any leftover live segment before committing catch-up.
      replaceBeforeCaret(el, live.length, "");
      resetSegment();
    }
    replaceBeforeCaret(el, 0, text);
    wroteFinal = true;
    return true;
  }

  /** Delete characters before the caret (used when Clear wipes typed transcript). */
  function retract(count: number): boolean {
    const el = targetElement();
    if (!el) return false;
    if (!isGoogleDocs) prepareCaret(el);
    const remove = Math.max(0, count) + live.length;
    if (remove) replaceBeforeCaret(el, remove, "");
    resetSegment();
    wroteFinal = false;
    return true;
  }

  chrome.runtime.onMessage.addListener((message: InserterMessage, _sender, sendResponse) => {
    if (message.type === "sac-sync") {
      sendResponse(sync(message.text, message.final));
    } else if (message.type === "sac-commit") {
      sendResponse(commit(message.text));
    } else if (message.type === "sac-retract") {
      sendResponse(retract(message.count));
    } else if (message.type === "sac-reset") {
      resetSegment();
      wroteFinal = false;
      sendResponse(true);
    }
  });

  scope.__sacInserter = {
    probe: () => {
      const editable = targetElement() !== null;
      return { editable, focusedAt: editable ? focusedAt : 0 };
    },
    alive: () => Boolean(chrome.runtime?.id),
  };
})();
