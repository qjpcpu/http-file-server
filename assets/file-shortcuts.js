(() => {
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
