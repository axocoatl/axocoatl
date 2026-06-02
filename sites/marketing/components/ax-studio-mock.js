/**
 * <ax-studio-mock></ax-studio-mock>
 *
 * A faithful mini-Studio screenshot built in HTML. The chrome
 * (strip, sidebar, toolbar) is identical to the dashboard's so a
 * visitor recognizes the product before the demo even starts.
 *
 * The scripted lattice inside is the canonical brand demo: a
 * three-agent bug-fix workflow. As the lattice emits `phase` events
 * we mirror them across:
 *   - the "Active runs" pill in the sidebar,
 *   - the per-agent rows in "All agents",
 *   - a floating inspector pane in the canvas with full detail.
 * The lattice is authoritative; this mock just listens.
 */
class AxStudioMock extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    this.innerHTML = `
      <div class="studio-mock">
        <aside class="studio-mock-strip">
          <div class="studio-mock-mark">
            <img src="/assets/mark.png" alt="" aria-hidden="true">
          </div>
          <div class="studio-mock-tab" title="Chat">✦</div>
          <div class="studio-mock-tab" title="Files">▤</div>
          <div class="studio-mock-tab" title="Sessions">▣</div>
          <div class="studio-mock-tab" title="Automations">⟳</div>
          <div class="studio-mock-tab active" title="Studio">◉</div>
          <div class="studio-mock-tab" title="Agents">⌬</div>
          <div class="studio-mock-tab" title="Skills">◈</div>
          <div class="studio-mock-tab" title="MCP">◇</div>
          <div class="studio-mock-spacer"></div>
          <div class="studio-mock-tab" title="Docs">◫</div>
        </aside>

        <div class="studio-mock-shell">
          <aside class="studio-mock-side">
            <div class="studio-mock-search">
              <span class="studio-mock-search-icon">⌕</span>
              Filter agents, teams…
            </div>

            <div class="studio-mock-section">
              <div class="studio-mock-section-head">▾ Active runs</div>
              <div class="studio-mock-row" data-active-row>
                <span class="studio-mock-dot" data-active-dot></span>
                <span class="studio-mock-row-name" data-active-name>idle</span>
              </div>
            </div>

            <div class="studio-mock-section">
              <div class="studio-mock-section-head">▾ Teams</div>
              <div class="studio-mock-row">
                <span class="studio-mock-dot team-eng"></span>
                <span class="studio-mock-row-name">Engineering</span>
                <span class="studio-mock-row-ct">3</span>
              </div>
              <div class="studio-mock-row">
                <span class="studio-mock-dot team-qa"></span>
                <span class="studio-mock-row-name">Customer</span>
                <span class="studio-mock-row-ct">2</span>
              </div>
              <div class="studio-mock-row">
                <span class="studio-mock-dot team-res"></span>
                <span class="studio-mock-row-name">Ops</span>
                <span class="studio-mock-row-ct">1</span>
              </div>
              <div class="studio-mock-row">
                <span class="studio-mock-dot team-eng"></span>
                <span class="studio-mock-row-name">Marketing</span>
                <span class="studio-mock-row-ct">1</span>
              </div>
            </div>

            <div class="studio-mock-section">
              <div class="studio-mock-section-head">▾ All agents</div>
              <div class="studio-mock-row" data-row="qa">
                <span class="studio-mock-dot" data-row-dot="qa"></span>
                <span class="studio-mock-row-name">qa-verifier</span>
              </div>
              <div class="studio-mock-row" data-row="engineer">
                <span class="studio-mock-dot" data-row-dot="engineer"></span>
                <span class="studio-mock-row-name">engineer</span>
              </div>
              <div class="studio-mock-row" data-row="reviewer">
                <span class="studio-mock-dot" data-row-dot="reviewer"></span>
                <span class="studio-mock-row-name">reviewer</span>
              </div>
              <div class="studio-mock-row">
                <span class="studio-mock-dot"></span>
                <span class="studio-mock-row-name">support</span>
              </div>
              <div class="studio-mock-row">
                <span class="studio-mock-dot"></span>
                <span class="studio-mock-row-name">analyst</span>
              </div>
              <div class="studio-mock-row">
                <span class="studio-mock-dot"></span>
                <span class="studio-mock-row-name">planner</span>
              </div>
              <div class="studio-mock-row">
                <span class="studio-mock-dot"></span>
                <span class="studio-mock-row-name">researcher</span>
              </div>
            </div>

            <div class="studio-mock-section">
              <div class="studio-mock-section-head">▾ Quick fire</div>
              <div class="studio-mock-row">
                <span class="studio-mock-row-name">▶ Fix bug 273</span>
              </div>
              <div class="studio-mock-row">
                <span class="studio-mock-row-name">▶ Feature: double-jump</span>
              </div>
              <div class="studio-mock-row">
                <span class="studio-mock-row-name">▶ Release pulse</span>
              </div>
            </div>

            <div class="studio-mock-section">
              <div class="studio-mock-section-head">▾ Recent events</div>
              <div class="studio-mock-event" data-event-row>
                <span class="studio-mock-event-time">just now</span>
                <span class="studio-mock-event-text" data-event-text>fix-bug-273 started</span>
              </div>
            </div>
          </aside>

          <main class="studio-mock-main">
            <div class="studio-mock-toolbar">
              <div class="studio-mock-mode">
                <button class="studio-mock-mode-btn active">◉ Watch</button>
                <button class="studio-mock-mode-btn">✎ Edit</button>
              </div>
              <span class="studio-mock-toolbar-help" data-toolbar-help>
                fix-bug-273 · running
              </span>
              <div class="studio-mock-toolbar-actions">
                <button class="studio-mock-toolbar-btn">Auto-layout</button>
                <button class="studio-mock-toolbar-btn">Fit</button>
                <button class="studio-mock-toolbar-btn">Reset</button>
              </div>
            </div>
            <div class="studio-mock-canvas">
              <ax-scripted-lattice></ax-scripted-lattice>

              <!-- Inspector pane — slides in when a node activates. -->
              <div class="studio-mock-inspector" data-inspector>
                <div class="studio-mock-inspector-head">
                  <span class="studio-mock-inspector-name" data-insp-name>—</span>
                  <span class="studio-mock-inspector-meta" data-insp-meta>node</span>
                </div>
                <div class="studio-mock-inspector-body">
                  <div class="studio-mock-inspector-section">
                    <div class="studio-mock-inspector-label">Status</div>
                    <div class="studio-mock-inspector-value">
                      <span class="studio-mock-dot run"></span>
                      <span data-insp-status>—</span>
                    </div>
                  </div>
                  <div class="studio-mock-inspector-section">
                    <div class="studio-mock-inspector-label">Action</div>
                    <div class="studio-mock-inspector-value" data-insp-action>—</div>
                  </div>
                  <div class="studio-mock-inspector-section">
                    <div class="studio-mock-inspector-label">Detail</div>
                    <div class="studio-mock-inspector-value" data-insp-detail>—</div>
                  </div>
                  <div class="studio-mock-inspector-section" data-insp-code-section>
                    <div class="studio-mock-inspector-label">Patch</div>
                    <pre class="studio-mock-inspector-code" data-insp-code></pre>
                  </div>
                </div>
              </div>

              <div class="studio-mock-pearls">
                <span class="studio-mock-pearl">
                  <span class="studio-mock-dot run"></span>
                  fix-bug-273 · running
                </span>
                <span class="studio-mock-pearl">
                  3 agents · 18 events
                </span>
              </div>
            </div>
          </main>
        </div>
      </div>
    `;

    // ── Listen to the lattice's phase events and mirror them ────────
    const $ = (sel) => this.querySelector(sel);

    const activeRow  = $('[data-active-row]');
    const activeDot  = $('[data-active-dot]');
    const activeName = $('[data-active-name]');
    const eventText  = $('[data-event-text]');
    const rows = {
      qa:       $('[data-row="qa"]'),
      engineer: $('[data-row="engineer"]'),
      reviewer: $('[data-row="reviewer"]'),
    };
    const dots = {
      qa:       $('[data-row-dot="qa"]'),
      engineer: $('[data-row-dot="engineer"]'),
      reviewer: $('[data-row-dot="reviewer"]'),
    };
    const insp = {
      pane:    $('[data-inspector]'),
      name:    $('[data-insp-name]'),
      meta:    $('[data-insp-meta]'),
      status:  $('[data-insp-status]'),
      action:  $('[data-insp-action]'),
      detail:  $('[data-insp-detail]'),
      code:    $('[data-insp-code]'),
      codeSec: $('[data-insp-code-section]'),
    };

    const NODE_TO_LABEL = {
      qa:       'qa-verifier',
      engineer: 'engineer',
      reviewer: 'reviewer',
    };

    const updateSidebar = (node) => {
      // Per-agent dots + row highlight
      Object.entries(rows).forEach(([k, row]) => {
        const on = k === node;
        row.classList.toggle('run', on);
        dots[k].classList.toggle('run', on);
      });
      // Active runs pill
      if (node) {
        activeRow.classList.add('run');
        activeDot.classList.add('run');
        activeName.textContent = NODE_TO_LABEL[node];
      } else {
        activeRow.classList.remove('run');
        activeDot.classList.remove('run');
        activeName.textContent = 'idle';
      }
    };

    const renderDiff = (s) => {
      if (!s) return '';
      return s.split('\n').map(line => {
        const cls =
          line.startsWith('+') ? 'diff-add' :
          line.startsWith('-') ? 'diff-del' : '';
        return cls
          ? `<span class="${cls}">${escapeHtml(line)}</span>`
          : escapeHtml(line);
      }).join('\n');
    };
    const escapeHtml = (s) => String(s).replace(/[&<>"']/g,
      c => ({ '&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;' }[c]));

    const updateInspector = (phase) => {
      if (!phase.node) {
        insp.pane.classList.remove('show');
        return;
      }
      insp.name.textContent   = NODE_TO_LABEL[phase.node];
      insp.meta.textContent   = `node · ${phase.node}`;
      insp.status.textContent = phase.status;
      insp.action.textContent = phase.action;
      insp.detail.textContent = phase.detail;
      if (phase.code) {
        insp.codeSec.style.display = '';
        insp.code.innerHTML = renderDiff(phase.code);
      } else {
        insp.codeSec.style.display = 'none';
      }
      insp.pane.classList.add('show');
    };

    const EVENT_TEXTS = {
      'qa-active':       'qa-verifier · BugReproduced',
      'engineer-active': 'engineer · PatchWritten',
      'reviewer-active': 'reviewer · PatchApproved',
      'idle':            'fix-bug-273 · complete',
    };
    const updateEvent = (phase) => {
      const txt = EVENT_TEXTS[phase.name];
      if (txt) eventText.textContent = txt;
    };

    // Initial: idle (the lattice hasn't fired anything yet)
    updateSidebar(null);
    insp.pane.classList.remove('show');

    this.addEventListener('phase', (e) => {
      const phase = e.detail;
      updateSidebar(phase.node);
      updateInspector(phase);
      updateEvent(phase);
    });
  }
}
customElements.define('ax-studio-mock', AxStudioMock);
