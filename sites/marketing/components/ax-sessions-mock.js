/**
 * <ax-sessions-mock></ax-sessions-mock>
 *
 * A faithful screenshot of the actual Sessions cockpit as it ships in
 * the Axocoatl dashboard.  The chrome (strip, title bar, 3 panes,
 * terminals rail, status pearls) mirrors the real product 1:1 — same
 * sections, same controls, same fonts, same colors.
 *
 * Layout:
 *   ┌─────┬───────────────────────────────────────────────────┬─────┐
 *   │     │  ← Sessions  name  path  · idle  Panes ▾         │     │
 *   │ S   ├──────────┬──────────────────────────────┬──────────┤  ◀ │
 *   │ T   │ FILES    │ ACTIVITY                     │ BROWSER  │ T  │
 *   │ R   │  ┌ tree  │ ┌ ACTIVE ● AGENT coder       │ url bar  │ E  │
 *   │ I   │  └ ed.   │ │ messages                   │ body     │ R  │
 *   │ P   │          │ │ ─────── model · pick  Send │          │ M  │
 *   └─────┴──────────┴──────────────────────────────┴──────────┴────┘
 */
class AxSessionsMock extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    this.innerHTML = `
      <div class="sx-mock">
        <!-- ─── Strip (left rail, 40px, icons identical to dashboard) ─── -->
        <aside class="sx-strip">
          <div class="sx-strip-mark"><img src="/assets/mark.png" alt=""></div>
          <div class="sx-strip-item" title="Chat">✦</div>
          <div class="sx-strip-item" title="Files">▤</div>
          <div class="sx-strip-item active" title="Sessions">▣</div>
          <div class="sx-strip-item" title="Automations">⟳</div>
          <div class="sx-strip-item" title="Studio">◉</div>
          <div class="sx-strip-item" title="Agents">⌬</div>
          <div class="sx-strip-item" title="Skills">◈</div>
          <div class="sx-strip-item" title="MCP">◇</div>
          <div class="sx-strip-spacer"></div>
          <div class="sx-strip-item" title="Docs">◫</div>
          <div class="sx-strip-pearl ok" title="daemon healthy"></div>
        </aside>

        <!-- ─── Cockpit ─── -->
        <div class="sx-cockpit">
          <!-- Title bar — back, name, dir, status, panes menu. -->
          <div class="sx-bar">
            <span class="sx-back">← Sessions</span>
            <strong class="sx-name">serpent-run-feature</strong>
            <span class="sx-dir">~/code/serpent-run</span>
            <span class="sx-mid"></span>
            <span class="sx-live"><span class="sx-live-dot"></span> live</span>
            <span class="sx-panes-btn">Panes ▾</span>
          </div>

          <!-- Shell: 3-pane grid + terminals rail. -->
          <div class="sx-shell">
            <div class="sx-grid">

              <!-- ── Files ── -->
              <section class="sx-pane">
                <header class="sx-pane-head">
                  <span class="sx-pane-title">Files</span>
                  <span class="sx-pane-meta">─</span>
                  <span class="sx-pane-btn">⛶</span>
                  <span class="sx-pane-btn">◀</span>
                </header>
                <div class="sx-files">
                  <div class="sx-file-search">
                    <span class="sx-file-search-ico">⌕</span>
                    <input class="sx-file-search-input" placeholder="Search files…" />
                  </div>
                  <div class="sx-file-tree">
                    <div class="sx-file dir open">▾ src</div>
                    <div class="sx-file indent" data-editing>jump.ts</div>
                    <div class="sx-file indent">boulder.ts</div>
                    <div class="sx-file indent">game.ts</div>
                    <div class="sx-file dir open">▾ tests</div>
                    <div class="sx-file indent">jump.test.ts</div>
                    <div class="sx-file">package.json</div>
                    <div class="sx-file">README.md</div>
                  </div>
                  <div class="sx-file-empty">Select a file to view</div>
                </div>
              </section>

              <!-- ── Activity (Stream) ── -->
              <section class="sx-pane sx-pane-stream">
                <header class="sx-pane-head">
                  <span class="sx-pane-title">Activity</span>
                  <span class="sx-pane-meta">─</span>
                  <span class="sx-pane-btn">⛶</span>
                </header>
                <!-- ACTIVE row — matches the real session-active strip. -->
                <div class="sx-active">
                  <span class="sx-active-eyebrow">active</span>
                  <span class="sx-chip">
                    <span class="sx-chip-dot"></span>
                    <span class="sx-chip-kind">agent</span>
                    <span class="sx-chip-name">coder</span>
                  </span>
                </div>
                <div class="sx-msgs">
                  <div class="sx-msg user">
                    <div class="sx-msg-role">you</div>
                    <div class="sx-msg-body">
                      Add a double-jump. Hold space to trigger; second
                      jump should clear two boulder widths.
                    </div>
                  </div>

                  <div class="sx-msg agent">
                    <div class="sx-msg-role">coder</div>
                    <div class="sx-msg-body">
                      Reading <code>jump.ts</code> + the boulder
                      definitions to see what the current jump returns.
                    </div>
                  </div>

                  <div class="sx-toolchip ok">
                    <span class="sx-toolchip-name">read_file</span>
                    <span class="sx-toolchip-arg">src/jump.ts</span>
                    <span class="sx-toolchip-meta">38 lines</span>
                  </div>

                  <div class="sx-toolchip ok">
                    <span class="sx-toolchip-name">read_file</span>
                    <span class="sx-toolchip-arg">src/boulder.ts</span>
                    <span class="sx-toolchip-meta">22 lines</span>
                  </div>

                  <div class="sx-msg agent">
                    <div class="sx-msg-role">coder</div>
                    <div class="sx-msg-body">
                      Adding a <code>double</code> returner that scales
                      at 0.65× when <code>held</code> is true. Updating
                      <code>jump.ts</code> + the test.
                    </div>
                  </div>

                  <div class="sx-toolchip ok">
                    <span class="sx-toolchip-name">write_file</span>
                    <span class="sx-toolchip-arg">src/jump.ts</span>
                    <span class="sx-toolchip-meta">+9 −2</span>
                  </div>

                  <div class="sx-toolchip running" data-tool-running>
                    <span class="sx-toolchip-name">bash</span>
                    <span class="sx-toolchip-arg">npm test</span>
                    <span class="sx-toolchip-spinner"></span>
                  </div>

                  <div class="sx-typing" data-typing>
                    <span></span><span></span><span></span>
                  </div>
                </div>
                <!-- Per-turn pickers + input row. -->
                <div class="sx-input">
                  <div class="sx-input-bar">
                    <span class="sx-pick">model · qwen2.5-coder:14b ▾</span>
                  </div>
                  <div class="sx-input-row">
                    <input class="sx-input-text" placeholder="Tell the agents what to build — Enter to send" />
                    <button class="sx-input-send">Send</button>
                  </div>
                </div>
              </section>

              <!-- ── Browser ── -->
              <section class="sx-pane">
                <header class="sx-pane-head">
                  <span class="sx-pane-title">Browser</span>
                  <span class="sx-pane-meta">─</span>
                  <span class="sx-pane-btn">⛶</span>
                  <span class="sx-pane-btn">▶</span>
                </header>
                <div class="sx-browser-tools">
                  <span class="sx-browser-btn">←</span>
                  <span class="sx-browser-btn">→</span>
                  <span class="sx-browser-btn">⟳</span>
                  <span class="sx-browser-url">http://localhost:5173</span>
                  <span class="sx-browser-btn pick">⊙ Pick</span>
                  <span class="sx-browser-btn">Go</span>
                </div>
                <div class="sx-browser-body">
                  Enter a URL above, or run a dev server in the
                  Terminals pane — detected URLs appear here as
                  one-click chips.
                </div>
              </section>

            </div>

            <!-- ─── Terminals rail (40px on the right) ─── -->
            <aside class="sx-term-rail">
              <button class="sx-term-toggle">◀</button>
              <div class="sx-term-rail-label">TERMINALS</div>
              <span class="sx-term-rail-add">＋</span>
            </aside>
          </div>
        </div>
      </div>
    `;
  }
}
customElements.define('ax-sessions-mock', AxSessionsMock);
