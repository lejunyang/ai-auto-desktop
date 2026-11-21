// Installed only while recording, in the page's world where its handlers
// call the API. Return the browser's original promise and file handle.
(() => {
  const signal = '__AAD_SAVE_SIGNAL__';
  const original = window.showSaveFilePicker;
  if (typeof original !== 'function') return;
  let serial = 0, active = true;
  const dispatch = EventTarget.prototype.dispatchEvent;
  const Event = CustomEvent;
  const send = (phase, call) => {
    if (active) Reflect.apply(dispatch, document, [new Event(signal, {detail: phase + ':' + call})]);
  };
  const then = Promise.prototype.then;
  const kind = Object.getOwnPropertyDescriptor(FileSystemHandle.prototype, 'kind').get;
  function showSaveFilePicker(...args) {
    const call = ++serial;
    send('start', call);
    try {
      const promise = Reflect.apply(original, this, args);
      Reflect.apply(then, promise, [
        handle => {
          let file = false;
          try { file = Reflect.apply(kind, handle, []) === 'file'; } catch {}
          send(file ? 'selected' : 'failed', call);
        },
        error => send(error?.name === 'AbortError' ? 'cancel' : 'failed', call)
      ]);
      return promise;
    } catch (error) {
      send('failed', call);
      throw error;
    }
  }
  window.showSaveFilePicker = showSaveFilePicker;
  document.addEventListener(signal + '_stop', () => {
    active = false;
    if (window.showSaveFilePicker === showSaveFilePicker) window.showSaveFilePicker = original;
  }, {once: true});
})()
