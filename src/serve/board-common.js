/* Shared by /board (conversations) and /tasks/<project>.
 * A page keeps only what is its own — its data, its card, its actions — and gets the rest
 * from here: storage, toast, POST, card drag-and-drop, lane folding, segmented toggles, the
 * right-click menu, the board switcher and the sticky column header. */
(function () {
  const $ = s => document.querySelector(s);
  const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

  for (const [rel, href] of [['icon', "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAABmJLR0QA/wD/AP+gvaeTAAAD+klEQVRYhcWWy24cRRSGv6q+TPdMt2faM57EhqBIiYKUBUZigwRIFi+AwgMkYc9D5FUilmxY8ABRFNYkQokgihSEhJP4MteevlRfDouxAwGPZiws+5RKap2qOv9ff9c5VYpj29mxt15Zd1DcRtTHwBpnaxOUPEbU/d3fovvwfQWgALZu7PQE/QOoz84YdJE9UlS3dp8/OFDs7Nibu/oBcF7gRyaPXj3v7Vhb9tVvgG/PFxyAD4Lu7KVdC3dALgAfgLs2SrYvCl1g2xaRsz7tp7GOfRr5G34TrxWAgng4pCoLACzbIYgiEMhmMXmarBzTXgVfa03Y7WHSlPH+HgBhFKGUBkCkZrw39/tBSLvXZ3p4QF3XywnICgzcVot4PKQ05q1vMhoSrvcBmA4POVYyiScYk+G2WqTTyXICyAoSiLztoizEboBtU3stAOpGAGWJKnOUVO/MX0pgFQVAEITashHXR1wPcVyqIACgyjJUYVDGQpmUeUxhldgrKdDwmoTRBnuHAwrHQ5o+TtTGun5zHsR6RjEco5SFoy263XVMOiOZjP8fgWa4hh+EjPbe4K51uXztBjGC9HrENz7lzyvb1Cg6Hzyj8/Qn9P5rAqUZ//GSbLxHp9vHmJxkMlqIYTU77907aSDoRBRFznQ4oEbI0YzyHH/zMsMv7jLYuk7Va1AHDkmjD71r+Lu/8Pr3lxTJFIqcLIlRWuEHASZLT6eA1hqTHOeznnetKeqKwvUgsGDDmQ8XQjHzKOoK9NFcUSA1Jknw/ObCA2kv+gHCuyVKTvAtWrNo7YkEFjGryxLX8zFpAlRQV1CWOErj5BnETdifV0LiCifPcJSGspzPPUpH129Sl+VCBSy/vXXvpAGTpTR8n3Z3gzyZ4bVC+leuUhhDY/QCz7EwuouOS3qvnxL8/CMMD9lYa1MnUyozo93rgwjxeLhQNbX+/idL8zC6tEXDb/LmYEjpekiziRO1CY7SMH5xlIZJgm0yLnUjTJYweLO7LPRqdSCfTZkO9qkrQZUFVAVFnlGX81pfHOyjTI7KM2qTcJBPcBve2VXC47pGZVBZBWUBeQoNFwA1HaHKElVmIBViuX+vWUbgtHcBUqJMjCoUOvEA0On43d2e4i7QwvKWzia0OhF+EL71hVGE1AVSF/Pvo+YHIa1ORDqbrBBZUJ3N7ZVfJI1mC78Vzh8kgwFlOb+ebdslWF8HgXQ2JU9mq4ZEdTY/urAXKYAtImOgfUH4Qw3y5L9F9Nz6E8tt9TXIV+ex3X+bKO4pwFrr33wAfH6e4AoejveefamBSuzyFsij85JeIQ9rp/4aqNQ/SFnhxoe3Qd0B2QbVOds9ywjUY+D+dP/X74AK4C/GJ1LQBq9HEwAAAABJRU5ErkJggg=="], ['apple-touch-icon', "data:image/png;base64,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"]]) {
    const l = document.createElement('link');
    l.rel = rel; if (rel === 'icon') l.type = 'image/png'; l.href = href;
    document.head.appendChild(l);
  }

  // Per-viewer conveniences only; a page works without storage.
  const store = prefix => ({
    get(k, d) { try { const v = localStorage.getItem(prefix + k); return v == null ? d : JSON.parse(v); } catch { return d; } },
    set(k, v) { try { localStorage.setItem(prefix + k, JSON.stringify(v)); } catch {} },
  });

  async function post(path, body) {
    const r = await fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    return r.json().catch(() => ({}));
  }

  function toast(msg) {
    const t = $('#toast');
    t.textContent = msg;
    t.classList.add('show');
    clearTimeout(toast.timer);
    toast.timer = setTimeout(() => t.classList.remove('show'), 2400);
  }

  // ---- drag & drop: a card (data-id) dropped on a cell (.cell) -> onDrop(id, cell) ----
  let dragging = null;
  function wireDnD(board, onDrop) {
    board.addEventListener('dragstart', e => {
      const card = e.target.closest('.card');
      if (!card) return;
      dragging = card.dataset.id;
      card.classList.add('dragging');
      e.dataTransfer.effectAllowed = 'move';
      e.dataTransfer.setData('text/plain', dragging);
    });
    board.addEventListener('dragend', e => {
      e.target.closest('.card')?.classList.remove('dragging');
      document.querySelectorAll('.cell.over').forEach(c => c.classList.remove('over'));
      dragging = null;
    });
    board.addEventListener('dragover', e => {
      const cell = e.target.closest('.cell');
      if (!cell || !dragging) return;
      e.preventDefault();
      document.querySelectorAll('.cell.over').forEach(c => c !== cell && c.classList.remove('over'));
      cell.classList.add('over');
    });
    board.addEventListener('dragleave', e => {
      const cell = e.target.closest('.cell');
      if (cell && !cell.contains(e.relatedTarget)) cell.classList.remove('over');
    });
    board.addEventListener('drop', e => {
      const cell = e.target.closest('.cell');
      if (!cell || !dragging) return;
      e.preventDefault();
      cell.classList.remove('over');
      const id = dragging;
      dragging = null;
      onDrop(id, cell);
    });
  }

  // A click on a lane header folds it; `folded` is the page's Set, `persist` saves it.
  function toggleLane(head, folded, persist) {
    const key = head.parentElement.dataset.lane;
    folded.has(key) ? folded.delete(key) : folded.add(key);
    persist();
    head.parentElement.classList.toggle('folded');
  }

  // Segmented toggle: buttons carry data-v.
  const seg = (sel, onPick) => $(sel).addEventListener('click', e => {
    const v = e.target.closest('button')?.dataset.v;
    if (v) onPick(v);
  });
  const segMark = (sel, isOn) =>
    document.querySelectorAll(sel + ' button').forEach(b => b.classList.toggle('on', isOn(b.dataset.v)));

  // ---- right-click menu: the page builds the HTML and handles the clicks ----
  const menu = {
    el: null,
    onClose: null,
    open(html, x, y) {
      const m = this.el;
      m.innerHTML = html;
      m.classList.add('open');
      const r = m.getBoundingClientRect();   // keep it on screen: flip left/up when it would overflow
      m.style.left = Math.max(8, Math.min(x, innerWidth - r.width - 8)) + 'px';
      m.style.top = Math.max(8, y + r.height > innerHeight - 8 ? y - r.height : y) + 'px';
    },
    close() { this.el.classList.remove('open'); if (this.onClose) this.onClose(); },
  };
  // This script is loaded at the end of <body>, after the page's markup, so these exist already.
  menu.el = $('#menu');
  document.addEventListener('mousedown', e => { if (!menu.el.contains(e.target)) menu.close(); });
  document.addEventListener('keydown', e => { if (e.key === 'Escape') menu.close(); });
  window.addEventListener('blur', () => menu.close());
  $('main').addEventListener('scroll', () => menu.close(), { passive: true });

  // A hairline under the pinned column names once the board has scrolled under them.
  function syncStuck(offset = () => 0) {
    document.querySelector('.cols')?.classList.toggle('stuck', $('main').scrollTop > offset());
  }

  // The board switcher beside the logo: Conversations plus every project's task board.
  async function switcher() {
    const sel = $('#boardSel');
    let boards = [];
    try { boards = await (await fetch('/api/task-boards')).json(); } catch {}
    sel.innerHTML = '<option value="/board">💬 Conversations</option>' + boards.map(b =>
      `<option value="/tasks/${encodeURIComponent(b.key)}">📋 ${esc(b.emoji)} ${esc(b.name)}</option>`).join('');
    sel.onchange = () => { location.href = sel.value; };
    return boards;
  }
  const pick = href => { $('#boardSel').value = href; };

  window.Board = { $, esc, store, post, toast, wireDnD, toggleLane, seg, segMark, menu, syncStuck, switcher, pick,
                   dragging: () => dragging };
})();
