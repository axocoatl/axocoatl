import { adopt } from './sheets.js';
import { SETTINGS_CSS, emit, h, jsonRequest } from './settings-common.js';

/**
 * `<ax-settings-skills>` owns Skills discovery, filtering, inspection and firing.
 * Firing a Skill publishes each event in its `emits` list on the event feed.
 *
 * @element ax-settings-skills
 * @fires skills-change detail: {skills}
 * @fires skill-fired  detail: {skill, result}
 * @fires notify       detail: {title, body, kind}
 */

const CSS = `${SETTINGS_CSS}
.columns, .skill-row {
  display: grid; grid-template-columns: minmax(190px, 1.6fr) 2fr;
  align-items: center; gap: var(--sp-3);
}
.columns {
  padding: 7px var(--sp-3); border-bottom: 1px solid var(--border);
  background: var(--panel-2); color: var(--muted); font-size: var(--fs-xs);
  letter-spacing: .06em; text-transform: uppercase;
}
.skill-row {
  width: 100%; padding: var(--sp-2) var(--sp-3); border: 0;
  border-bottom: 1px solid var(--border); background: transparent;
  color: var(--text); cursor: pointer; text-align: left;
}
.skill-row:hover, .skill-row[aria-current="true"] { background: var(--bg-2); }
.name { min-width: 0; }
.name strong, .name span { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.name strong { font-size: var(--fs-body); font-weight: var(--fw-medium); }
.name span { margin-top: 2px; color: var(--muted); font-size: var(--fs-xs); }
.chips { display: flex; min-width: 0; flex-wrap: wrap; gap: var(--sp-1); overflow: hidden; }
.chip {
  display: inline-flex; max-width: 100%; padding: 2px 6px; border-radius: var(--r-sm);
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  font: var(--fs-xs) var(--font-mono);
}
.chip.emit { color: var(--axo-blue); background: rgba(var(--axo-blue-rgb), .12); }
.more { color: var(--muted-2); font: var(--fs-xs) var(--font-mono); }
.drawer-copy { margin: 0 0 var(--sp-4); color: var(--muted); font-size: var(--fs-sm); line-height: var(--lh-body); }
.drawer-chips { display: flex; flex-wrap: wrap; gap: var(--sp-1); }
@media (max-width: 760px) {
  .columns, .skill-row { grid-template-columns: minmax(150px, 1.4fr) 1fr; gap: var(--sp-2); }
}
@media (max-width: 560px) {
  .columns { display: none; }
  .skill-row { grid-template-columns: minmax(0, 1fr) auto; }
  .skill-row > .chips { display: none; }
}
`;

export class AxSettingsSkills extends HTMLElement {
  #root;
  #skills = [];
  #phase = 'idle';
  #error = '';
  #operationError = '';
  #event = '';
  #query = '';
  #sideQuery = '';
  #selected = '';
  #generation = 0;
  #busySkill = '';

  constructor() {
    super();
    this.#root = this.attachShadow({ mode: 'open' });
    this.#root.innerHTML = `
      <div class="shell">
        <aside class="side">
          <div class="side-search"><input class="search side-query" type="search" placeholder="Filter skills…" spellcheck="false"></div>
          <div class="side-scroll">
            <section class="side-section" data-group="event">
              <button class="side-head" type="button"><span class="tri">▾</span><span>By event</span></button>
              <div class="side-list event-list"></div>
            </section>
          </div>
        </aside>
        <main class="main">
          <div class="toolbar"><h2>Skills</h2><span class="sub count-label"></span><span class="grow"></span><input class="search row-query" type="search" placeholder="Filter…" spellcheck="false"></div>
          <div class="errors" role="status" aria-live="polite"></div>
          <div class="columns"><span>Name</span><span>◆ Emits</span></div>
          <div class="content rows"></div>
          <aside class="drawer" aria-label="Skill details">
            <div class="drawer-head"><h3>Skill</h3><button class="icon-button close" type="button" title="Close">×</button></div>
            <div class="drawer-body"></div>
            <div class="drawer-foot"><button class="action primary fire" type="button">◆ Fire this Skill</button></div>
          </aside>
        </main>
      </div>`;
    adopt(this.#root, CSS);
    this.#wire();
  }

  connectedCallback() {
    if (this.#phase === 'idle') void this.refresh();
  }

  get skills() {
    return this.#skills.map((skill) => ({
      ...skill,
      emits: [...(skill.emits || [])],
    }));
  }

  async refresh() {
    const generation = ++this.#generation;
    this.#phase = 'loading';
    this.#error = '';
    this.#renderErrors();
    this.#root.querySelector('.shell').classList.add('loading');
    try {
      const skills = await jsonRequest('/api/skills');
      if (!Array.isArray(skills)) throw new Error('The Skills endpoint returned an invalid list.');
      if (generation !== this.#generation) return;
      this.#skills = skills;
      this.#phase = 'ready';
      if (this.#selected && !skills.some((skill) => skill.id === this.#selected)) this.#selected = '';
      this.#render();
      emit(this, 'skills-change', { skills: this.skills });
    } catch (error) {
      if (generation !== this.#generation) return;
      this.#phase = 'error';
      this.#error = String(error?.message || error);
      this.#renderErrors();
    } finally {
      if (generation === this.#generation) this.#root.querySelector('.shell').classList.remove('loading');
    }
  }

  openSkill(skillOrId) {
    const id = typeof skillOrId === 'object' ? skillOrId?.id : skillOrId;
    if (!id) return false;
    if (typeof skillOrId === 'object' && !this.#skills.some((skill) => skill.id === id)) {
      this.#skills = [...this.#skills, skillOrId];
    }
    if (!this.#skills.some((skill) => skill.id === id)) return false;
    this.#selected = id;
    this.#renderRows();
    this.#renderDrawer();
    queueMicrotask(() => this.#root.querySelector('.drawer .close')?.focus());
    return true;
  }

  closeSkill() {
    this.#selected = '';
    this.#renderRows();
    this.#renderDrawer();
  }

  async fire(skillOrId) {
    let skill = typeof skillOrId === 'object'
      ? skillOrId
      : this.#skills.find((candidate) => candidate.id === skillOrId);
    if (!skill && typeof skillOrId === 'string') {
      await this.refresh();
      skill = this.#skills.find((candidate) => candidate.id === skillOrId);
    }
    if (!skill?.id) {
      this.#operationError = `Skill “${String(skillOrId || '')}” is not available. Refresh and try again.`;
      this.#renderErrors();
      return null;
    }
    if (this.#busySkill) return null;
    this.#busySkill = skill.id;
    this.#renderDrawer();
    try {
      const result = await jsonRequest(`/api/skills/${encodeURIComponent(skill.id)}/fire`, { method: 'POST' });
      if (!Array.isArray(result?.events_published)) {
        throw new Error('The Skill fire endpoint returned an invalid result.');
      }
      this.#operationError = '';
      const count = result.events_published.length;
      emit(this, 'notify', {
        title: `Fired '${skill.name}'`,
        body: `Published ${count} event${count === 1 ? '' : 's'}`,
        kind: 'ok',
      });
      emit(this, 'skill-fired', { skill: { ...skill }, result });
      if (this.#selected === skill.id) this.closeSkill();
      return result;
    } catch (error) {
      this.#operationError = `Could not fire ${skill.name}: ${error?.message || error}`;
      this.#renderErrors();
      emit(this, 'notify', { title: 'Fire failed', body: String(error?.message || error), kind: 'err' });
      return null;
    } finally {
      this.#busySkill = '';
      this.#renderDrawer();
    }
  }

  #wire() {
    this.#root.querySelector('.side-query').addEventListener('input', (event) => {
      this.#sideQuery = event.target.value.trim().toLowerCase();
      this.#renderSidebar();
    });
    this.#root.querySelector('.row-query').addEventListener('input', (event) => {
      this.#query = event.target.value.trim().toLowerCase();
      this.#renderRows();
    });
    this.#root.querySelectorAll('.side-head').forEach((button) => {
      button.addEventListener('click', () => button.closest('.side-section').toggleAttribute('data-collapsed'));
    });
    this.#root.querySelector('.drawer .close').addEventListener('click', () => this.closeSkill());
    this.#root.querySelector('.drawer .fire').addEventListener('click', () => {
      if (this.#selected) void this.fire(this.#selected);
    });
    this.#root.addEventListener('keydown', (event) => {
      if (event.key === 'Escape' && this.#selected) {
        event.preventDefault();
        event.stopPropagation();
        this.closeSkill();
      }
    });
  }

  #render() {
    this.#renderErrors();
    this.#renderSidebar();
    this.#renderRows();
    this.#renderDrawer();
  }

  #renderErrors() {
    const host = this.#root.querySelector('.errors');
    host.replaceChildren();
    if (this.#error) {
      const row = h('div', 'error');
      row.append(h('strong', '', 'Skills could not be loaded'));
      const retry = h('button', '', 'Retry');
      retry.type = 'button';
      retry.addEventListener('click', () => void this.refresh());
      row.append(retry, h('span', '', this.#error));
      host.append(row);
    }
    if (this.#operationError) {
      const row = h('div', 'error');
      row.append(h('strong', '', 'Skill action failed'), h('span', '', this.#operationError));
      host.append(row);
    }
  }

  #renderSidebar() {
    const match = (value) => !this.#sideQuery || String(value || '').toLowerCase().includes(this.#sideQuery);
    const events = new Map();
    this.#skills.forEach((skill) => {
      new Set(skill.emits || []).forEach((event) => events.set(event, (events.get(event) || 0) + 1));
    });

    const eventHost = this.#root.querySelector('.event-list');
    eventHost.replaceChildren(this.#sideRow('⊕', 'All skills', this.#skills.length, !this.#event, () => {
      this.#event = ''; this.#renderSidebar(); this.#renderRows();
    }));
    [...events.keys()].sort().filter(match).forEach((event) => {
      eventHost.append(this.#sideRow('◆', event, events.get(event), this.#event === event, () => {
        this.#event = this.#event === event ? '' : event;
        this.#renderSidebar(); this.#renderRows();
      }));
    });
  }

  #sideRow(icon, label, count, current, action) {
    const row = h('button', 'side-row');
    row.type = 'button';
    row.setAttribute('aria-current', String(current));
    row.append(h('span', 'ico', icon), h('span', 'label', label));
    if (count !== '') row.append(h('span', 'count', count));
    row.addEventListener('click', action);
    return row;
  }

  #visibleSkills() {
    return this.#skills.filter((skill) => {
      if (this.#event && !(skill.emits || []).includes(this.#event)) return false;
      if (!this.#query) return true;
      return [skill.id, skill.name, skill.description, ...(skill.emits || [])]
        .some((value) => String(value || '').toLowerCase().includes(this.#query));
    });
  }

  #renderRows() {
    const host = this.#root.querySelector('.rows');
    host.replaceChildren();
    const visible = this.#visibleSkills();
    this.#root.querySelector('.count-label').textContent = `${visible.length} of ${this.#skills.length}`;
    if (!visible.length) {
      host.append(h('div', 'empty', this.#phase === 'loading' ? 'Loading Skills…' : 'No Skills match.'));
      return;
    }
    visible.forEach((skill) => {
      const row = h('button', 'skill-row');
      row.type = 'button';
      row.setAttribute('aria-current', String(this.#selected === skill.id));
      const name = h('span', 'name');
      name.append(h('strong', '', skill.name || skill.id), h('span', '', skill.description || ''));
      row.append(name, this.#chipCell(skill.emits, 'emit'));
      row.addEventListener('click', () => this.openSkill(skill.id));
      host.append(row);
    });
  }

  #chipCell(items, kind) {
    const cell = h('span', 'chips');
    (items || []).slice(0, 3).forEach((item) => cell.append(h('span', `chip ${kind}`, item)));
    if ((items || []).length > 3) cell.append(h('span', 'more', `+${items.length - 3}`));
    if (!(items || []).length) cell.append(h('span', 'more', '—'));
    return cell;
  }

  #renderDrawer() {
    const drawer = this.#root.querySelector('.drawer');
    const skill = this.#skills.find((candidate) => candidate.id === this.#selected);
    drawer.toggleAttribute('data-open', Boolean(skill));
    if (!skill) return;
    drawer.querySelector('h3').textContent = skill.id;
    const body = drawer.querySelector('.drawer-body');
    body.replaceChildren(h('p', 'drawer-copy', skill.description || 'No description.'));
    const section = h('section', 'section');
    section.append(h('h4', '', '◆ Emits'));
    const chips = h('div', 'drawer-chips');
    (skill.emits || []).forEach((item) => chips.append(h('span', 'chip emit', item)));
    if (!(skill.emits || []).length) chips.append(h('span', 'muted', 'none — firing publishes nothing'));
    section.append(chips);
    body.append(section);
    const fire = drawer.querySelector('.fire');
    fire.disabled = Boolean(this.#busySkill);
    fire.textContent = this.#busySkill === skill.id ? 'Firing…' : '◆ Fire this Skill';
  }
}

if (!customElements.get('ax-settings-skills')) {
  customElements.define('ax-settings-skills', AxSettingsSkills);
}
