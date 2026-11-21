(() => {
  if (globalThis.__aadCdp?.version === 3) return true;
  globalThis.__aadCdp?.stop?.();
  const binding = '__AAD_BINDING__';
  const documentId = globalThis.crypto?.randomUUID?.() || String(Date.now()) + Math.random();
  const ids = new WeakMap();
  let nextId = 0, sequence = 0, dropped = 0, dirty = true;
  const queue = [], listeners = [];
  const id = el => { if (!ids.has(el)) ids.set(el, ++nextId); return String(ids.get(el)); };
  /*AAD_NAMING*/
  const label = el => (el.getAttribute?.('aria-label') ||
    Array.from(el.labels || []).map(l => l.innerText || l.textContent || '').join(' ') ||
    el.getAttribute?.('title') || el.getAttribute?.('placeholder') ||
    (['INPUT','TEXTAREA','SELECT'].includes(el.tagName) ? el.id : (el.innerText || el.textContent)) || '').trim().replace(/\s+/g, ' ').slice(0, 180);
  const role = el => el.getAttribute?.('role') || ({BUTTON:'button',A:'link',INPUT:el.type==='checkbox'?'checkbox':'textbox',TEXTAREA:'textbox',SELECT:'combobox',IMG:'img',CANVAS:'canvas',DIALOG:'dialog',OUTPUT:'status'}[el.tagName] || el.tagName?.toLowerCase());
  const protectedField = el => el.tagName === 'INPUT' && ['password','file','hidden'].includes((el.type || '').toLowerCase());
  const element = el => ({
    node_id: id(el), role: role(el), name: label(el), tag: el.tagName?.toLowerCase(),
    locator: {role:role(el),name:label(el),id:el.id || null,test_id:el.getAttribute?.('data-testid') || null},
    // The role and name the control session finds it by, for replaying the step.
    control: {role:roleOf(el),name:labelOf(el)},
    enabled: !el.disabled && el.getAttribute?.('aria-disabled') !== 'true',
    selected: el.getAttribute?.('aria-selected'), expanded: el.getAttribute?.('aria-expanded'),
    checked: typeof el.checked === 'boolean' ? el.checked : el.getAttribute?.('aria-checked'),
    protected: protectedField(el),
    value: protectedField(el) ? null : (el.value == null ? null : String(el.value).slice(0, 500)),
    value_truncated: !protectedField(el) && String(el.value || '').length > 500,
    href: el.href || null, src: ['IMG','CANVAS'].includes(el.tagName) ? (el.currentSrc || el.src || null) : null
  });
  const describe = el => {
    const target = element(el);
    const container=el.parentElement?.closest('tr[aria-label], [role="row"][aria-label], form[aria-label], section[aria-label], dialog[aria-label], [role="group"][aria-label]');
    if(container && labelOf(container)) target.control.within={role:roleOf(container),name:labelOf(container)};
    return target;
  };
  const emit = (kind, el, value = null, trusted = false, domEvent = kind, target = describe(el)) => {
    const event = {kind, ...target, tag:el.isContentEditable ? 'contenteditable' : target.tag, label:target.name, type:el.type || null,
      value, truncated: value != null && value.length > 500,
      url:location.href, document_id:documentId, sequence:++sequence,
      at_ms:Date.now(), monotonic_ms:performance.now(), trusted, dom_event:domEvent};
    if (event.value != null) event.value = event.value.slice(0,500);
    if (binding && typeof globalThis[binding] === 'function') {
      globalThis[binding](JSON.stringify(event));
    } else {
      const previous = queue[queue.length-1];
      // Only adjacent edits of the SAME DOM element are one editing segment.
      if (kind === 'input' && previous?.kind === 'input' && previous.node_id === event.node_id) {
        queue[queue.length-1] = event;
      } else {
        if (queue.length === 512) { queue.shift(); dropped++; }
        queue.push(event);
      }
    }
    dirty = true;
    return event.sequence;
  };
  const listen = (name, handler) => { document.addEventListener(name, handler, true); listeners.push([name,handler]); };
  const targetOf = event => (event.composedPath?.()[0] || event.target);
  const isFile = el => el?.tagName === 'INPUT' && el.type === 'file';
  const fileTargets = new WeakMap();
  const usedFileTargets = new WeakSet();
  let activation, enter;
  const describeFile = el => {
    const hidden = !el.getClientRects().length || getComputedStyle(el).visibility === 'hidden';
    if (!el.files?.length && !usedFileTargets.has(el)) fileTargets.set(el, describe(el));
    return {...(fileTargets.get(el) || {node_id:id(el),tag:'input',control:{},protected:true}),hidden};
  };
  listen('click', event => {
    const origin = targetOf(event);
    const el = origin?.closest?.('button,a,input,select,[role="button"],[role="tab"],[tabindex]') || origin;
    const label = origin?.closest?.('label');
    const field = isFile(origin) ? origin : (!origin?.closest?.('button,a,input,select,textarea') && label?.control);
    if (isFile(field)) {
      const target = describeFile(field);
      const trigger = activation?.event.eventPhase ? activation.sequence : null;
      emit('file_activation', field, null, event.isTrusted, 'click', {...target,trigger_sequence:trigger});
    } else if (el?.tagName) {
      const enter_sequence = event.isTrusted && event.detail === 0 && enter?.element === el ? enter.sequence : undefined;
      activation = {event, sequence:emit('click', el, null, event.isTrusted, 'click', {...describe(el),enter_sequence})};
    }
  });
  listen('submit', event => { if (event.target?.tagName) emit('submit', event.target, null, event.isTrusted); });
  const onInput = event => {
    const el = targetOf(event);
    if (!el?.tagName || (!['INPUT','TEXTAREA','SELECT'].includes(el.tagName) && !el.isContentEditable)) return;
    if (isFile(el)) {
      if (!event.isTrusted || !['input','cancel'].includes(event.type)) return;
      if (el.files?.length) usedFileTargets.add(el);
      const target = describeFile(el);
      const kind = event.type === 'cancel' ? 'file_cancel' : el.webkitdirectory ? 'file_directory' : !el.files?.length ? 'file_empty' : target.control.role ? 'upload' : 'file_target_unobserved';
      emit(kind, el, null, true, event.type, target);
      return;
    }
    const value = protectedField(el) ? null : String(el.isContentEditable ? (el.innerText || el.textContent || '') : (el.value || ''));
    emit('input', el, value, event.isTrusted, event.type);
  };
  listen('input', onInput); listen('change', onInput); listen('compositionend', onInput);
  listen('cancel', event => { if (isFile(targetOf(event))) onInput(event); });
  // Enter without a form submit still establishes an ordering boundary.
  listen('keydown', event => {
    const el = targetOf(event);
    enter = null;
    if (event.key === 'Enter' && !event.isComposing && el?.tagName && !isFile(el)) {
      activation = {event, sequence:emit('enter', el, null, event.isTrusted)};
      if (event.isTrusted) enter = {element:el,sequence:activation.sequence};
    }
  });
  listen('keyup', event => { if (event.key === 'Enter') enter = null; });
  const observer = typeof MutationObserver === 'function' ? new MutationObserver(() => { dirty = true; }) : null;
  if (binding) observer?.observe(document, {subtree:true,childList:true,attributes:true,characterData:true});
  const snapshot = () => {
    const nodes = [], roots = [document];
    let scanned = 0, truncated = false;
    for (let r=0; r<roots.length && r<64; r++) {
      const walker = document.createTreeWalker(roots[r], NodeFilter.SHOW_ELEMENT);
      let el;
      while ((el=walker.nextNode())) {
        if (++scanned > 10000) { truncated=true; break; }
        if (el.shadowRoot) roots.push(el.shadowRoot);
        const semantic=el.matches('button,a,input,textarea,select,[role],[aria-live],dialog,output,h1,h2,h3,img,canvas,td,th,dt,dd,label,p,[data-testid]');
        const leaf=!el.children.length && !['SCRIPT','STYLE','NOSCRIPT','OPTION'].includes(el.tagName) && (el.textContent || '').trim();
        if (!semantic && !leaf) continue;
        const file = isFile(el) ? describeFile(el) : null;
        if (file ? file.hidden : !el.getClientRects().length || getComputedStyle(el).visibility === 'hidden') continue;
        const rect=el.getBoundingClientRect();
        if (rect.bottom<0 || rect.right<0 || rect.top>innerHeight || rect.left>innerWidth) continue;
        if (nodes.length >= 100) { truncated=true; break; }
        nodes.push(file || element(el));
      }
      if (truncated) break;
    }
    dirty = false;
    return {document_id:documentId,url:location.href,title:document.title,nodes,truncated};
  };
  globalThis.__aadCdp = {
    version:3, installed:true, snapshot, isDirty:()=>dirty,
    take:()=>queue.splice(0),
    takeReport:()=>{ const report={events:queue.splice(0),dropped}; dropped=0; return report; },
    stop:()=>{ for (const [name,fn] of listeners) document.removeEventListener(name,fn,true); observer?.disconnect(); queue.length=0; delete globalThis.__aadCdp; }
  };
  return true;
})()
