from pathlib import Path

path = Path('crates/daemon/src/settings/settings.html')
text = path.read_text(encoding='utf-8')
old = '''    if (cell.classList.contains('open')) {
      pop.querySelectorAll('.menu-item.subopen').forEach(function(m) { m.classList.remove('subopen'); });
    }
    if (!cell.classList.contains('open')) {
      /* Right-anchor to the button so the flyout grows inward (it lives in the
         right-side Options column) and never clips the right edge. */
      var rect = btn.getBoundingClientRect();
      pop.style.right = Math.max(8, window.innerWidth - rect.right) + 'px';
      pop.style.left = 'auto';
      if (window.innerHeight - rect.bottom - 8 >= Math.min(pop.scrollHeight + 4, 420)) {
        pop.style.top = (rect.bottom + 4) + 'px'; pop.style.bottom = 'auto';
      } else {
        pop.style.bottom = (window.innerHeight - rect.top + 4) + 'px'; pop.style.top = 'auto';
      }
    } else {
      updateRuleSummary(tr);
    }
    cell.classList.toggle('open');
'''
new = '''    if (cell.classList.contains('open')) {
      pop.querySelectorAll('.menu-item.subopen').forEach(function(m) { m.classList.remove('subopen'); });
      cell.classList.remove('open');
      updateRuleSummary(tr);
      return;
    }

    /* Make the flyout measurable, then clamp its fixed rectangle to the
       viewport. Measuring while display:none reports scrollHeight=0 and made
       the menu overflow the bottom edge in the minimum-size Settings window. */
    cell.classList.add('open');
    var margin = 8;
    var rect = btn.getBoundingClientRect();
    pop.style.maxHeight = Math.max(120, window.innerHeight - margin * 2) + 'px';
    var popupWidth = pop.offsetWidth;
    var popupHeight = Math.min(pop.scrollHeight, window.innerHeight - margin * 2);
    var left = rect.right - popupWidth;
    left = Math.max(margin, Math.min(left, window.innerWidth - popupWidth - margin));
    var top = rect.bottom + 4;
    if (top + popupHeight > window.innerHeight - margin) {
      top = rect.top - popupHeight - 4;
    }
    top = Math.max(margin, Math.min(top, window.innerHeight - popupHeight - margin));
    pop.style.left = left + 'px';
    pop.style.right = 'auto';
    pop.style.top = top + 'px';
    pop.style.bottom = 'auto';
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f'Expected one legacy rule-popup positioning block, found {count}')
path.write_text(text.replace(old, new), encoding='utf-8', newline='\n')
print('Settings rule-popup positioning hardened')
