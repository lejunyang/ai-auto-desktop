(() => {
  if (globalThis.__aadControlV1) return true;
  const documentToken = NONCE;
  let observation = null;
  let nodes = new Map();
  const roleOf = el => el.getAttribute('role') || ({INPUT:'textbox',TEXTAREA:'textbox',BUTTON:'button',A:'link',SELECT:'combobox',BODY:'document'}[el.tagName] || 'generic');
  const textOf = el => {let text='',i=0;const walker=document.createTreeWalker(el,NodeFilter.SHOW_TEXT);let node;while(text.length<256 && i++<32 && (node=walker.nextNode()))text+=(node.nodeValue||'').slice(0,256-text.length);return text;};
  const labelOf = el => (el.getAttribute('aria-label') || (el.labels && Array.from(el.labels).slice(0,8).map(textOf).join(' ')) || el.getAttribute('title') || el.getAttribute('placeholder') || (['INPUT','TEXTAREA','SELECT'].includes(el.tagName) ? el.id : textOf(el)) || '').trim().slice(0,256);
  const protectedNode = el => el.tagName === 'INPUT' && ['password','file','hidden'].includes((el.type||'').toLowerCase());
  const visible = el => el.isConnected && el.ownerDocument === document && el.getClientRects().length > 0 && getComputedStyle(el).visibility !== 'hidden';
  const enabled = el => !el.disabled && !el.closest('[inert]') && el.getAttribute('aria-disabled') !== 'true';
  const editable = el => (['INPUT','TEXTAREA'].includes(el.tagName) || el.isContentEditable) && !el.readOnly && !protectedNode(el);
  const fingerprint = el => JSON.stringify([el.tagName,el.id,el.type||'',roleOf(el),labelOf(el),el.getAttribute('name'),el.getAttribute('href'),!!el.isContentEditable]);
  const actions = el => {
    if (!enabled(el) || protectedNode(el)) return [];
    const result=[];
    if (typeof el.focus==='function') result.push('focus','key');
    if ((['BUTTON','A','INPUT','SELECT'].includes(el.tagName) || el.onclick || roleOf(el)==='button') && typeof el.click==='function') result.push('click');
    if (editable(el)) result.push('fill');
    return result;
  };
  const check = args => {
    if (args.document_token!==documentToken || args.observation!==observation) return {error:'CONTROL.STALE_TARGET'};
    const found=nodes.get(args.node_id);
    if (!found || !visible(found.el) || fingerprint(found.el)!==found.fingerprint) return {error:'CONTROL.STALE_TARGET'};
    if (!enabled(found.el) || protectedNode(found.el)) return {error:'CONTROL.ACTION_UNSUPPORTED'};
    if (!actions(found.el).includes(args.action)) return {error:'CONTROL.ACTION_UNSUPPORTED'};
    return found;
  };
  globalThis.__aadControlV1 = {
    observe: (id,limit) => {
      observation=id; nodes=new Map();
      const items=[];
      const walker=document.body?document.createTreeWalker(document.body,NodeFilter.SHOW_ELEMENT):null;
      let el=document.body,scanned=0;
      while(el && items.length<=limit && scanned++<20000) {if(visible(el))items.push(el);el=walker.nextNode();}
      const result=items.slice(0,limit).map((el,index)=>{
        const node_id='n'+index;
        nodes.set(node_id,{el,fingerprint:fingerprint(el)});
        return {node_id,role:roleOf(el),name:labelOf(el),tag:el.tagName.toLowerCase(),
          value:protectedNode(el)?null:(('value' in el)?String(el.value).slice(0,4096):null),
          states:{enabled:enabled(el),protected:protectedNode(el),read_only:!!el.readOnly,focused:document.activeElement===el},actions:actions(el)};
      });
      return {document_token:documentToken,nodes:result,truncated:items.length>limit||el!==null};
    },
    act: args => {
      const found=check(args); if(found.error) return found;
      const el=found.el;
      if(args.action==='click') { el.click(); return {submitted:true,native_operation:'HTMLElement.click',trusted_event:false}; }
      if(args.action==='fill') {
        if(el.isContentEditable) el.textContent=args.value;
        else {
          const prototype=el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;
          Object.getOwnPropertyDescriptor(prototype,'value').set.call(el,args.value);
        }
        el.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
        el.dispatchEvent(new Event('change',{bubbles:true,composed:true}));
        return {submitted:true,native_operation:'DOM.value+input+change'};
      }
      if(args.action==='focus'||args.action==='key') {
        el.focus({preventScroll:true});
        if(document.activeElement!==el) return {error:'CONTROL.FOCUS_FAILED',effect:'contextual'};
        return {submitted:true,native_operation:'HTMLElement.focus'};
      }
      return {error:'CONTROL.ACTION_UNSUPPORTED'};
    },
    verify: args => {
      const found=check(args); if(found.error) return found;
      return {ok:document.activeElement===found.el};
    }
  };
  return true;
})()
