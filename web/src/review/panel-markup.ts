import { icon } from "./icons";
import { escapeHtml } from "./markup";

/**
 * The review panel's static skeleton. Controllers find their parts by id and data attribute, so
 * every id here is part of the panel's contract with them.
 */
export function panelMarkup(repository: string, settingsPopover: string, rangeDialog: string) {
  return `
      <div class="review-shell">
        <header class="topbar">
          <nav class="tabs" role="tablist" aria-label="Review sections">
            <button class="tab active" id="changes-tab" role="tab" aria-selected="true" aria-controls="changes-panel" data-tab="changes">Changes <span id="file-count">0</span></button>
            <button class="tab" id="overview-tab" role="tab" aria-selected="false" aria-controls="overview-panel" tabindex="-1" data-tab="overview">Overview<span class="activity-spinner overview-tab-activity" aria-hidden="true"></span></button>
          </nav>
          <button class="range-button" id="range-button" aria-haspopup="dialog" aria-controls="range-dialog" aria-expanded="false" aria-label="Change range">
            ${icon("git-branch")}
            <strong id="range-label">Full branch</strong>
            <span class="range-chevron">${icon("chevron-down")}</span>
          </button>
          <div class="change-stats" id="change-stats" aria-label="Change statistics"></div>
          <p class="scope-description" id="scope-description" title="${escapeHtml(repository)}">Loading changes…</p>
          <div class="topbar-actions">
            <div class="live-status" role="status" aria-live="polite">
              <span class="live-badge" id="live-badge" title="The agent is working; this diff follows its edits." hidden><i aria-hidden="true"></i>Live</span>
              <span class="updating" id="updating" hidden><span class="activity-spinner" aria-hidden="true"></span>Updating</span>
            </div>
            <button class="refresh-notice" id="refresh-notice" hidden>
              <i aria-hidden="true"></i><span>New changes available</span><strong>Refresh</strong>
            </button>
            <div class="layout-toggle" role="group" aria-label="Diff layout">
              <button type="button" data-diff-style="unified">Unified</button>
              <button type="button" data-diff-style="split">Split</button>
            </div>
            <div class="view-toggles" role="group" aria-label="Changes navigation">
              <button class="icon-button" type="button" aria-pressed="false" aria-label="Files" data-mobile-panel="files">${icon("list")}</button>
              <button class="icon-button" type="button" aria-pressed="false" aria-label="Comments" data-mobile-panel="comments">${icon("comment")}<span class="count-badge" id="mobile-comment-count" hidden>0</span></button>
            </div>
            ${settingsPopover}
          </div>
        </header>
        <section class="panel overview-panel" id="overview-panel" role="tabpanel" aria-labelledby="overview-tab" data-panel="overview" hidden>
          <div class="overview-state" id="overview-state"></div>
          <iframe class="overview" title="Agent overview" sandbox="allow-scripts" hidden></iframe>
        </section>
        <section class="panel changes-panel active" id="changes-panel" role="tabpanel" aria-labelledby="changes-tab" data-panel="changes">
          <div class="review-search" id="review-search" role="search" hidden>
            <label class="sr-only" for="review-search-input">Find in changes</label>
            <input id="review-search-input" type="search" placeholder="Find in changes" autocomplete="off" spellcheck="false" aria-describedby="review-search-count">
            <span id="review-search-count" aria-live="polite">0 of 0</span>
            <button class="small-icon-button search-previous" data-search-previous aria-label="Previous match" title="Previous match" disabled>${icon("chevron-down")}</button>
            <button class="small-icon-button" data-search-next aria-label="Next match" title="Next match" disabled>${icon("chevron-down")}</button>
            <button class="small-icon-button" data-search-close aria-label="Close search" title="Close search">${icon("close")}</button>
          </div>
          <aside class="sidebar">
            <div class="files-navigation" data-mobile-content="files">
              <div class="sidebar-label">Changed files</div>
              <div id="file-tree" class="file-tree"></div>
            </div>
            <div class="comment-index" data-mobile-content="comments">
              <div class="sidebar-label">Comments <span id="comment-count">0</span></div>
              <div id="comment-list" class="comment-list">
                <p class="empty-comments">Select a line in the diff to comment.</p>
              </div>
            </div>
          </aside>
          <main id="diff-view" class="diff-view" data-mobile-content="diff" tabindex="-1"></main>
        </section>
        <footer class="review-bar">
          <label class="sr-only" for="review-summary">Overall review comment</label>
          <textarea id="review-summary" rows="1" placeholder="Leave an overall comment (optional)" aria-describedby="review-status"></textarea>
          <div class="review-actions">
            <button class="button secondary" id="ai-review" data-agent-action>AI review</button>
            <button class="button secondary" data-decision="approve">Approve</button>
            <button class="button primary" data-decision="request_changes">Send to chat</button>
          </div>
        </footer>
        <div class="review-status" id="review-status" role="status" aria-live="polite"></div>
        <div class="sr-only" id="agent-announcement" aria-live="polite"></div>
        <div class="scope-state" id="scope-state" role="status" aria-live="polite" hidden></div>
        ${rangeDialog}
      </div>`;
}
