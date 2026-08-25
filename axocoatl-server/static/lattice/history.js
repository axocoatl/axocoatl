/**
 * Undo / redo — a command-pattern history stack.
 *
 * A command is a plain object `{ label, undo, redo }` where `undo` and `redo`
 * are zero-arg functions that apply the inverse / forward mutation directly
 * (typically DOM operations). Commands are pushed by the lattice at the
 * high-level operation sites (move, delete, connect, paste).
 *
 * @module @axocoatl/lattice/history
 */

/**
 * @typedef {Object} Command
 * @property {string}   label   Human-readable description (for debugging)
 * @property {Function} undo    Applies the inverse mutation
 * @property {Function} redo    Re-applies the forward mutation
 */

export class History {
  /** @type {Command[]} */
  #undo = [];
  /** @type {Command[]} */
  #redo = [];
  /** Max depth before the oldest commands are dropped. */
  #limit;
  /** @type {(state: {canUndo:boolean,canRedo:boolean}) => void} */
  #onChange;

  /**
   * @param {(state: {canUndo:boolean,canRedo:boolean}) => void} [onChange]
   * @param {number} [limit]
   */
  constructor(onChange, limit = 200) {
    this.#onChange = onChange || (() => {});
    this.#limit = limit;
  }

  /** Record a freshly-applied command. Clears the redo stack. */
  push(command) {
    this.#undo.push(command);
    if (this.#undo.length > this.#limit) this.#undo.shift();
    this.#redo.length = 0;
    this.#emit();
  }

  canUndo() { return this.#undo.length > 0; }
  canRedo() { return this.#redo.length > 0; }

  /** Most recent command's label, or null. */
  peekUndo() { return this.#undo.at(-1)?.label ?? null; }
  peekRedo() { return this.#redo.at(-1)?.label ?? null; }

  /** Undo the most recent command. */
  undo() {
    const c = this.#undo.pop();
    if (!c) return false;
    c.undo();
    this.#redo.push(c);
    this.#emit();
    return true;
  }

  /** Redo the most recently undone command. */
  redo() {
    const c = this.#redo.pop();
    if (!c) return false;
    c.redo();
    this.#undo.push(c);
    this.#emit();
    return true;
  }

  /** Drop all history. */
  clear() {
    this.#undo.length = 0;
    this.#redo.length = 0;
    this.#emit();
  }

  #emit() {
    this.#onChange({ canUndo: this.canUndo(), canRedo: this.canRedo() });
  }
}
