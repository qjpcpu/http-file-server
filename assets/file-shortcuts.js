(() => {
  document.querySelector('#history-back')?.addEventListener('click', () => history.back());

  const breadcrumbs = document.querySelector('.file-breadcrumbs');
  if (breadcrumbs) {
    const links = Array.from(breadcrumbs.querySelectorAll('a'));
    links.forEach(link => { link.title = link.textContent; });
    const middle = links.slice(1, -1);
    const overflow = document.createElement('span');
    overflow.className = 'breadcrumb-overflow';
    overflow.hidden = true;
    overflow.innerHTML = '<span aria-hidden="true">/</span><details><summary aria-label="展开中间路径" title="展开中间路径">…</summary><div class="breadcrumb-menu"></div></details>';
    const menu = overflow.querySelector('.breadcrumb-menu');
    middle.forEach(link => menu.append(link.cloneNode(true)));
    const separators = middle.map(link => link.previousElementSibling);
    links[0].after(overflow);
    const details = overflow.querySelector('details');
    const fitBreadcrumbs = () => {
      details.open = false;
      overflow.hidden = true;
      middle.forEach((link, index) => { link.hidden = separators[index].hidden = false; });
      breadcrumbs.classList.add('measuring');
      const collapsed = middle.length > 0 && breadcrumbs.scrollWidth > breadcrumbs.clientWidth;
      middle.forEach((link, index) => { link.hidden = separators[index].hidden = collapsed; });
      overflow.hidden = !collapsed;
      breadcrumbs.classList.remove('measuring');
    };
    new ResizeObserver(fitBreadcrumbs).observe(breadcrumbs);
    fitBreadcrumbs();
    document.addEventListener('click', event => {
      if (!overflow.contains(event.target)) details.open = false;
    });
    document.addEventListener('keydown', event => {
      if (event.key === 'Escape' && details.open) {
        details.open = false;
        details.querySelector('summary').focus();
      }
    });
  }

  let pendingPath = null;
  let keyTimer;
  let toastTimer;
  let toast;

  const reset = () => {
    clearTimeout(keyTimer);
    pendingPath = null;
  };

  const notify = message => {
    if (!toast) {
      toast = document.createElement('div');
      toast.setAttribute('role', 'status');
      toast.id = 'file-path-toast';
      toast.style.cssText = 'position:fixed;z-index:100;right:1rem;bottom:1rem;max-width:calc(100vw - 2rem);padding:.7rem .9rem;border:1px solid var(--line,#dfe3ee);border-radius:.55rem;color:var(--ink,#202333);background:var(--surface,#fff);box-shadow:0 10px 35px rgba(0,0,0,.16);font:14px/1.5 system-ui,sans-serif;overflow-wrap:anywhere;pointer-events:none';
      document.body.append(toast);
    }
    toast.textContent = message;
    toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { toast.hidden = true; }, 2200);
  };

  const copyPath = path => {
    const focused = document.activeElement;
    const selection = window.getSelection();
    const ranges = Array.from({length: selection.rangeCount}, (_, index) => selection.getRangeAt(index).cloneRange());
    const field = document.createElement('textarea');
    field.value = path;
    field.readOnly = true;
    field.style.cssText = 'position:fixed;left:-9999px;top:0';
    document.body.append(field);
    field.select();
    let copied = false;
    try {
      copied = document.execCommand('copy');
    } catch {
      copied = false;
    } finally {
      field.remove();
      focused?.focus({preventScroll: true});
      selection.removeAllRanges();
      ranges.forEach(range => selection.addRange(range));
    }
    notify(copied ? `复制文件路径 ${path}` : '复制失败，请检查浏览器剪贴板权限');
  };

  const openReviewFile = document.querySelector('#open-review-file');
  if (openReviewFile) openReviewFile.href = `${location.pathname}.review.json`;
  document.querySelector('#copy-review-path')?.addEventListener('click', () => {
    copyPath(`${document.body.dataset.filePath}.review.json`);
  });

  document.addEventListener('keydown', event => {
    if (document.querySelector('#editor:not([hidden])')) {
      reset();
      return;
    }
    const target = event.target;
    if (event.defaultPrevented || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || event.isComposing || event.repeat ||
        target.closest('input, textarea, select') || target.isContentEditable || document.querySelector('dialog[open]')) {
      reset();
      return;
    }
    const lightbox = document.querySelector('#image-lightbox:not([hidden])');
    const path = lightbox ? lightbox.dataset.filePath : document.body.dataset.filePath;
    if (event.key !== 'y' || !path) {
      reset();
      return;
    }
    event.preventDefault();
    if (pendingPath === path) {
      reset();
      copyPath(path);
    } else {
      reset();
      pendingPath = path;
      keyTimer = setTimeout(reset, 500);
    }
  });
  window.addEventListener('blur', reset);
})();
