/**
 * <ax-automation-card
 *   trigger="schedule"
 *   trigger-detail="every Monday 9am"
 *   name="Release pulse"
 *   agents="activity-collector → issue-summarizer → release-writer → reviewer"
 *   status="enabled">
 *   <p>Body copy describing the workflow.</p>
 * </ax-automation-card>
 *
 * Looks like a real Automation card from the dashboard. The trigger
 * attribute picks the right glyph + label.
 */
const TRIGGER_META = {
  manual:   { glyph: '▶',  label: 'Manual'    },
  schedule: { glyph: '⏱', label: 'Scheduled' },
  event:    { glyph: '⊛',  label: 'Event'     },
  skill:    { glyph: '◇',  label: 'Skill'     },
  session:  { glyph: '▣',  label: 'Session'   },
};

class AxAutomationCard extends HTMLElement {
  connectedCallback() {
    if (this._wired) return;
    this._wired = true;
    const trigger = (this.getAttribute('trigger') || 'manual').toLowerCase();
    const meta = TRIGGER_META[trigger] || TRIGGER_META.manual;
    const triggerDetail = this.getAttribute('trigger-detail') || '';
    const name = this.getAttribute('name') || '';
    const agents = this.getAttribute('agents') || '';
    const status = this.getAttribute('status') || 'enabled';

    // The slot content is the body description — preserve any markup the
    // author wrote (we don't sanitize beyond what HTML parsing already did).
    const body = this.innerHTML;

    this.classList.add('automation-card');
    this.innerHTML = `
      <div class="automation-card-head">
        <span class="automation-trigger trigger-${trigger}">
          <span class="automation-trigger-glyph">${meta.glyph}</span>
          <span class="automation-trigger-label">${meta.label}</span>
          ${triggerDetail ? `<span class="automation-trigger-detail">· ${this._escape(triggerDetail)}</span>` : ''}
        </span>
        <span class="automation-status status-${status}">
          <span class="automation-status-dot"></span>
          ${this._escape(status)}
        </span>
      </div>
      <h3 class="automation-name">${this._escape(name)}</h3>
      <div class="automation-body">${body}</div>
      <div class="automation-chain">
        ${this._renderChain(agents)}
      </div>
    `;
  }

  _renderChain(s) {
    if (!s) return '';
    return s.split('→').map(part => part.trim())
      .map((p, i, arr) => {
        const chip = `<span class="automation-agent">${this._escape(p)}</span>`;
        return i < arr.length - 1 ? `${chip}<span class="automation-arrow">→</span>` : chip;
      }).join('');
  }

  _escape(s) {
    return String(s).replace(/[&<>"']/g, c => ({
      '&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'
    }[c]));
  }
}
customElements.define('ax-automation-card', AxAutomationCard);
