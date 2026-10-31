(() => {
  if (globalThis.__aadControlV1) return true;
  const documentToken = NONCE;
  let observation = null;
  let nodes = new Map();
  const inputRole = el => ({checkbox:'checkbox',radio:'radio',button:'button',submit:'button',reset:'button',range:'slider',file:'button'}[(el.type||'').toLowerCase()] || 'textbox');
  const roleOf = el => el.getAttribute('role') || (el.tagName==='INPUT' ? inputRole(el) : ({TEXTAREA:'textbox',BUTTON:'button',A:'link',SELECT:'combobox',BODY:'document',IMG:'img',CANVAS:'canvas',SVG:'img',LI:'listitem',UL:'list',OL:'list',TABLE:'table',TR:'row',TD:'cell',TH:'columnheader',H1:'heading',H2:'heading',H3:'heading',H4:'heading',H5:'heading',H6:'heading',DIALOG:'dialog',FORM:'form',HEADER:'banner',NAV:'navigation',MAIN:'main',ASIDE:'complementary',FOOTER:'contentinfo',SECTION:'region',OUTPUT:'status'}[el.tagName.toUpperCase()] || 'generic'));
  const textOf = el => {let text='',i=0;const walker=document.createTreeWalker(el,NodeFilter.SHOW_TEXT);let node;while(text.length<256 && i++<32 && (node=walker.nextNode()))text+=(node.nodeValue||'').slice(0,256-text.length);return text;};
  const labelOf = el => (el.getAttribute('aria-label') || (el.labels && Array.from(el.labels).slice(0,8).map(textOf).join(' ')) || el.getAttribute('alt') || el.getAttribute('title') || el.getAttribute('placeholder') || (['INPUT','TEXTAREA','SELECT'].includes(el.tagName) ? el.id : textOf(el)) || '').trim().slice(0,256);
  // Landmarks a page is divided into, as an overview names its regions.
  const LANDMARK_ROLES = ['banner','navigation','main','complementary','contentinfo','form','search','dialog','alertdialog','region'];
  const landmark = el => el!==document.body && (el.getAttribute('role') ? LANDMARK_ROLES.includes(el.getAttribute('role')) : (['HEADER','NAV','MAIN','ASIDE','FOOTER','FORM','DIALOG'].includes(el.tagName) || (el.tagName==='SECTION' && !!(el.getAttribute('aria-label')||el.getAttribute('aria-labelledby')))));
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
  // What is really under a point: elementFromPoint stops at a shadow host.
  const deepHit = (x,y) => {let hit=document.elementFromPoint(x,y);while(hit && hit.shadowRoot){const inner=hit.shadowRoot.elementFromPoint(x,y);if(!inner||inner===hit)break;hit=inner;}return hit;};
  // Containment across shadow boundaries.
  const inside = (el,hit) => {for(let n=hit;n;n=n.parentNode||n.host){if(n===el)return true;}return false;};
  const scrollable = el => el===document.body || ((el.scrollHeight>el.clientHeight+1 || el.scrollWidth>el.clientWidth+1) && /(auto|scroll|overlay)/.test(getComputedStyle(el).overflow+getComputedStyle(el).overflowY+getComputedStyle(el).overflowX));
  // What a person could pick up: HTML5 draggables, items of lists and grids,
  // and whatever shows a grab cursor.
  const draggable = el => el!==document.body && (el.draggable || ['listitem','option','row','treeitem'].includes(roleOf(el)) || /^(grab|grabbing|move)$/.test(getComputedStyle(el).cursor));
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
    if (draggable(el)) result.push('drag');
    if (el===document.body) result.push('navigate','back','forward','reload','open_tab','activate_page','close_page','emulate','clear_site_data');
    return result;
  };
  const check = args => {
    if (args.document_token!==documentToken || args.observation!==observation) return {error:'CONTROL.STALE_TARGET'};
    const found=nodes.get(args.node_id);
    if (!found || !present(found.el) || fingerprint(found.el)!==found.fingerprint) return {error:'CONTROL.STALE_TARGET'};
    if (!enabled(found.el) || protectedNode(found.el)) return {error:'CONTROL.ACTION_UNSUPPORTED'};
    // A drag's destination is any observed element; everything else needs its action.
    if (args.action!==null && !actions(found.el).includes(args.action)) return {error:'CONTROL.ACTION_UNSUPPORTED'};
    return found;
  };
  globalThis.__aadControlV1 = {
    observe: (id,limit) => {
      observation=id; nodes=new Map();
      const items=[];
      // Depth first through the page and every open shadow root: a host's
      // shadow tree first, then its light children, which show only where
      // slotted. Closed shadow roots cannot be entered from script.
      const stack=document.body?[document.body]:[];
      let scanned=0;
      while(stack.length && items.length<=limit && scanned++<20000) {
        const el=stack.pop();
        if(present(el))items.push(el);
        const children=el.shadowRoot?[...el.shadowRoot.children,...el.children]:el.children;
        for(let i=children.length-1;i>=0;i--)stack.push(children[i]);
      }
      const ids=new Map();
      const kept=items.slice(0,limit);
      kept.forEach((el,index)=>ids.set(el,'n'+index));
      // The nearest observed ancestor, across shadow hosts.
      const parentOf=el=>{for(let n=el.parentNode||el.host;n;n=n.parentNode||n.host){if(ids.has(n))return ids.get(n);}return null;};
      const result=kept.map((el,index)=>{
        const node_id='n'+index;
        nodes.set(node_id,{el,fingerprint:fingerprint(el)});
        const node={node_id,role:roleOf(el),name:labelOf(el),tag:el.tagName.toLowerCase(),
          value:protectedNode(el)?null:(('value' in el)?String(el.value).slice(0,4096):null),
          states:{enabled:enabled(el),protected:protectedNode(el),read_only:!!el.readOnly,focused:activeElement()===el},actions:actions(el),parent:parentOf(el)};
        if (landmark(el)) node.landmark=true;
        if (checkable(el)) node.states.checked=checkedOf(el);
        if (fileInput(el)) { node.states.hidden=!visible(el); node.files=Array.from(el.files||[]).slice(0,32).map(f=>f.name.slice(0,256)); node.multiple=!!el.multiple; }
        if (el.tagName==='SELECT') node.options=Array.from(el.options).slice(0,100).map(o=>o.label.trim().slice(0,256));
        return node;
      });
      // Top to bottom, then left to right: the order "the third button" means.
      const order=kept.map((el,index)=>{const box=el.getBoundingClientRect();return [Math.round(box.top+scrollY),Math.round(box.left+scrollX),index];})
        .sort((a,b)=>a[0]-b[0]||a[1]-b[1]||a[2]-b[2]).map(entry=>'n'+entry[2]);
      return {document_token:documentToken,nodes:result,order,truncated:items.length>limit||stack.length>0};
    },
    // The observed elements in node order, for the browser's accessibility tree.
    elements: () => Array.from(nodes.values()).map(found=>found.el),
    // What the page, or one observed region of it, shows as text, with tables
    // as rows of cells. Each part stops at max_characters.
    text: args => {
      if (args.document_token!==documentToken || args.observation!==observation) return {error:'CONTROL.STALE_TARGET'};
      let root=document.body;
      if (args.node_id) {
        const found=nodes.get(args.node_id);
        if (!found || !found.el.isConnected || fingerprint(found.el)!==found.fingerprint) return {error:'CONTROL.STALE_TARGET'};
        root=found.el;
      }
      const max=args.max_characters;
      const full=root ? (root.innerText||'') : '';
      const ids=new Map(); nodes.forEach((found,id)=>ids.set(found.el,id));
      let budget=max, cut=false;
      const cell=c=>{const text=(c.innerText||'').trim().slice(0,500);if(text.length>budget){cut=true;const part=text.slice(0,Math.max(budget,0));budget=0;return part;}budget-=text.length;return text;};
      const all=root ? Array.from(root.querySelectorAll('table')).filter(visible) : [];
      const tables=[];
      for (const table of all.slice(0,20)) {
        if (budget<=0) {cut=true;break;}
        const rows=Array.from(table.rows);
        const shown=[];
        for (const row of rows.slice(0,200)) {if(budget<=0){cut=true;break;}shown.push(Array.from(row.cells).slice(0,50).map(cell));}
        tables.push({node_id:ids.get(table)||null,name:(table.getAttribute('aria-label')||(table.caption?table.caption.innerText:'')||'').trim().slice(0,256),
          row_count:rows.length,rows:shown,truncated:shown.length<rows.length});
      }
      return {text:full.slice(0,max),characters:full.length,text_truncated:full.length>max,tables,table_count:all.length,tables_truncated:cut||all.length>tables.length};
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
    // Whether the pointer is over the observed element, as this frame sees it.
    hovered: args => {
      const found=check(args); if(found.error) return found;
      return {ok:found.el.matches(':hover')};
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
      if(!args.noscroll) el.scrollIntoView({block:'nearest',inline:'nearest'});
      const box=el.getBoundingClientRect();
      // Only the part inside the viewport can receive input; the body of a long page is mostly outside it.
      const left=Math.max(box.left,0), top=Math.max(box.top,0);
      const width=Math.min(box.right,innerWidth)-left, height=Math.min(box.bottom,innerHeight)-top;
      if(width<2 || height<2) return {error:'CONTROL.OCCLUDED'};
      const at=(start,size,f)=>Math.min(start+size-1,Math.max(start+1,start+size*f));
      const fx=args.fraction?args.fraction.fx:0.5, fy=args.fraction?args.fraction.fy:0.5;
      const x=at(left,width,fx), y=at(top,height,fy);
      if(!inside(el,deepHit(x,y))) return {error:'CONTROL.OCCLUDED'};
      return {x,y};
    }
  };
  return true;
})()
