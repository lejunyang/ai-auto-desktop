(() => {
  if (globalThis.__aadControlV1) return true;
  const documentToken = NONCE;
  let observation = null;
  let nodes = new Map();
  const inputRole = el => ({checkbox:'checkbox',radio:'radio',button:'button',submit:'button',reset:'button',range:'slider',file:'button'}[(el.type||'').toLowerCase()] || 'textbox');
  const roleOf = el => el.getAttribute('role') || (el.tagName==='INPUT' ? inputRole(el) : ({TEXTAREA:'textbox',BUTTON:'button',A:'link',SELECT:'combobox',BODY:'document',IMG:'img',CANVAS:'canvas',SVG:'img',LI:'listitem',UL:'list',OL:'list',TABLE:'table',TR:'row',TD:'cell',TH:'columnheader',H1:'heading',H2:'heading',H3:'heading',H4:'heading',H5:'heading',H6:'heading',DIALOG:'dialog',FORM:'form'}[el.tagName.toUpperCase()] || 'generic'));
  const textOf = el => {let text='',i=0;const walker=document.createTreeWalker(el,NodeFilter.SHOW_TEXT);let node;while(text.length<256 && i++<32 && (node=walker.nextNode()))text+=(node.nodeValue||'').slice(0,256-text.length);return text;};
  const labelOf = el => (el.getAttribute('aria-label') || (el.labels && Array.from(el.labels).slice(0,8).map(textOf).join(' ')) || el.getAttribute('alt') || el.getAttribute('title') || el.getAttribute('placeholder') || (['INPUT','TEXTAREA','SELECT'].includes(el.tagName) ? el.id : textOf(el)) || '').trim().slice(0,256);
  const protectedNode = el => el.tagName === 'INPUT' && ['password','hidden'].includes((el.type||'').toLowerCase());
  // A file input only ever takes files, through upload; often it is hidden behind a styled button.
  const fileInput = el => el.tagName === 'INPUT' && (el.type||'').toLowerCase() === 'file';
  const present = el => visible(el) || (fileInput(el) && el.isConnected && el.ownerDocument === document);
  const visible = el => el.isConnected && el.ownerDocument === document && el.getClientRects().length > 0 && getComputedStyle(el).visibility !== 'hidden';
  const enabled = el => !el.disabled && !el.closest('[inert]') && el.getAttribute('aria-disabled') !== 'true';
  const editable = el => (['INPUT','TEXTAREA'].includes(el.tagName) || el.isContentEditable) && !el.readOnly && !protectedNode(el) && !fileInput(el);
  const fingerprint = el => JSON.stringify([el.tagName,el.id,el.type||'',roleOf(el),labelOf(el),el.getAttribute('name'),el.getAttribute('href'),!!el.isContentEditable]);
  const checkable = el => (el.tagName==='INPUT' && ['checkbox','radio'].includes((el.type||'').toLowerCase())) || ['checkbox','switch','radio'].includes(el.getAttribute('role'));
  const checkedOf = el => el.tagName==='INPUT' ? !!el.checked : el.getAttribute('aria-checked')==='true';
  const radio = el => (el.type||'').toLowerCase()==='radio' || el.getAttribute('role')==='radio';
  // The focused element itself, inside any open shadow roots on the way.
  const activeElement = () => {let a=document.activeElement;while(a && a.shadowRoot && a.shadowRoot.activeElement)a=a.shadowRoot.activeElement;return a;};
  const scrollable = el => el===document.body || ((el.scrollHeight>el.clientHeight+1 || el.scrollWidth>el.clientWidth+1) && /(auto|scroll|overlay)/.test(getComputedStyle(el).overflow+getComputedStyle(el).overflowY+getComputedStyle(el).overflowX));
  const actions = el => {
    if (!enabled(el) || protectedNode(el)) return [];
    if (fileInput(el)) return ['upload'];
    const result=[];
    if (typeof el.focus==='function') result.push('focus','key');
    if ((['BUTTON','A','INPUT','SELECT'].includes(el.tagName) || el.onclick || roleOf(el)==='button' || checkable(el)) && typeof el.click==='function') result.push('click','hover');
    if (editable(el)) result.push('fill','type_text');
    if (el.tagName==='SELECT' && !el.multiple) result.push('select');
    if (checkable(el)) result.push('check');
    if (scrollable(el)) result.push('scroll');
    if (el===document.body) result.push('navigate','back','forward','reload','open_tab','activate_page','close_page');
    return result;
  };
  const check = args => {
    if (args.document_token!==documentToken || args.observation!==observation) return {error:'CONTROL.STALE_TARGET'};
    const found=nodes.get(args.node_id);
    if (!found || !present(found.el) || fingerprint(found.el)!==found.fingerprint) return {error:'CONTROL.STALE_TARGET'};
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
      while(el && items.length<=limit && scanned++<20000) {if(present(el))items.push(el);el=walker.nextNode();}
      const result=items.slice(0,limit).map((el,index)=>{
        const node_id='n'+index;
        nodes.set(node_id,{el,fingerprint:fingerprint(el)});
        const node={node_id,role:roleOf(el),name:labelOf(el),tag:el.tagName.toLowerCase(),
          value:protectedNode(el)?null:(('value' in el)?String(el.value).slice(0,4096):null),
          states:{enabled:enabled(el),protected:protectedNode(el),read_only:!!el.readOnly,focused:activeElement()===el},actions:actions(el)};
        if (checkable(el)) node.states.checked=checkedOf(el);
        if (fileInput(el)) { node.states.hidden=!visible(el); node.files=Array.from(el.files||[]).slice(0,32).map(f=>f.name.slice(0,256)); node.multiple=!!el.multiple; }
        if (el.tagName==='SELECT') node.options=Array.from(el.options).slice(0,100).map(o=>o.label.trim().slice(0,256));
        return node;
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
      if(args.action==='select') {
        const options=Array.from(el.options);
        const index=options.findIndex(o=>o.label.trim()===args.option);
        const chosen=index>=0?index:options.findIndex(o=>o.value===args.option);
        if(chosen<0 || options[chosen].disabled) return {error:'CONTROL.OPTION_NOT_FOUND'};
        el.selectedIndex=chosen;
        el.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
        el.dispatchEvent(new Event('change',{bubbles:true,composed:true}));
        return {submitted:true,native_operation:'HTMLSelectElement.selectedIndex+input+change',selected:options[chosen].label.trim()};
      }
      if(args.action==='check') {
        if(checkedOf(el)===args.checked) return {submitted:false,unchanged:true,checked:args.checked};
        // A radio is cleared by choosing another one, never by clicking it again.
        if(radio(el) && !args.checked) return {error:'CONTROL.ACTION_UNSUPPORTED'};
        el.click();
        if(checkedOf(el)!==args.checked) return {error:'CONTROL.STATE_UNCHANGED',effect:'contextual'};
        return {submitted:true,native_operation:'HTMLElement.click',checked:args.checked};
      }
      if(args.action==='focus'||args.action==='key'||args.action==='type_text') {
        if(activeElement()!==el) el.focus({preventScroll:true});
        if(activeElement()!==el) return {error:'CONTROL.FOCUS_FAILED',effect:'contextual'};
        return {submitted:true,native_operation:'HTMLElement.focus'};
      }
      return {error:'CONTROL.ACTION_UNSUPPORTED'};
    },
    // Keystrokes go wherever focus is: only to the observed element, in a focused document.
    verify: args => {
      const found=check(args); if(found.error) return found;
      return {ok:activeElement()===found.el && document.hasFocus()};
    },
    confirm: args => {
      const found=check(args); if(found.error) return found;
      return {ok:true};
    },
    // The checked element itself, by reference, for a protocol call that needs the node.
    element: args => {
      const found=check(args); return found.error ? null : found.el;
    },
    // The element's box in page CSS pixels, scrolled into view, for a screenshot.
    rect: args => {
      if (args.document_token!==documentToken || args.observation!==observation) return {error:'CONTROL.STALE_TARGET'};
      const found=nodes.get(args.node_id);
      if (!found || !visible(found.el) || fingerprint(found.el)!==found.fingerprint) return {error:'CONTROL.STALE_TARGET'};
      if (protectedNode(found.el)) return {error:'CONTROL.ACTION_UNSUPPORTED'};
      found.el.scrollIntoView({block:'nearest',inline:'nearest'});
      const box=found.el.getBoundingClientRect();
      if (box.width<1 || box.height<1) return {error:'CONTROL.OCCLUDED'};
      return {x:box.left+scrollX,y:box.top+scrollY,width:box.width,height:box.height};
    },
    // Where trusted pointer input should land, in this frame's CSS pixels. The
    // point must hit the observed element, so nothing laid over it is clicked.
    point: args => {
      const found=check(args); if(found.error) return found;
      const el=found.el;
      el.scrollIntoView({block:'nearest',inline:'nearest'});
      const box=el.getBoundingClientRect();
      // Only the part inside the viewport can receive input; the body of a long page is mostly outside it.
      const left=Math.max(box.left,0), top=Math.max(box.top,0);
      const width=Math.min(box.right,innerWidth)-left, height=Math.min(box.bottom,innerHeight)-top;
      if(width<2 || height<2) return {error:'CONTROL.OCCLUDED'};
      const at=(start,size,f)=>Math.min(start+size-1,Math.max(start+1,start+size*f));
      const fx=args.fraction?args.fraction.fx:0.5, fy=args.fraction?args.fraction.fy:0.5;
      const x=at(left,width,fx), y=at(top,height,fy);
      const hit=document.elementFromPoint(x,y);
      if(!hit || !(hit===el || el.contains(hit))) return {error:'CONTROL.OCCLUDED'};
      return {x,y};
    }
  };
  return true;
})()
