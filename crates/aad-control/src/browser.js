(() => {
  if (globalThis.__aadControlV1) return true;
  const documentToken = NONCE;
  let observation = null;
  let nodes = new Map();
  let scopeRoot = null, scopeInvalid = false, scopeNeedsValidation = false;
  let scopeParents = [], scopeElements = [], scopeIdentity = null;
  const scopeChanges = records => {
    if (!scopeRoot) return;
    for (const record of records) {
      // Changes inside the captured region, to its ancestors' attributes,
      // or moving the region itself invalidate its complete observation.
      if (inside(scopeRoot,record.target) ||
          (record.type==='attributes' && inside(record.target,scopeRoot)) ||
          (record.type==='childList' && [...record.addedNodes,...record.removedNodes].some(n=>inside(n,scopeRoot)))) {
        scopeInvalid=true;
      } else {
        // A sibling timer is not a change to this region. It may still move
        // it, reveal a previously hidden child through CSS, or rename it via
        // an external label: check the captured region before accepting it.
        scopeNeedsValidation=true;
      }
    }
  };
  const scopeObserver = new MutationObserver(scopeChanges);
  /*AAD_NAMING*/
  // Landmarks a page is divided into, as an overview names its regions.
  const LANDMARK_ROLES = ['banner','navigation','main','complementary','contentinfo','form','search','dialog','alertdialog','region'];
  const landmark = el => el!==document.body && (el.getAttribute('role') ? LANDMARK_ROLES.includes(el.getAttribute('role')) : (['HEADER','NAV','MAIN','ASIDE','FOOTER','FORM','DIALOG'].includes(el.tagName) || (el.tagName==='SECTION' && !!(el.getAttribute('aria-label')||el.getAttribute('aria-labelledby')))));
  // Retain protection for these elements even if the application reveals a
  // password by changing its input type. No secret values are stored here.
  const secretElements = new Set();
  // This private identity set survives observation resets in this named
  // isolated world. Detached fields no longer block screenshots.
  const closedSecrets = globalThis.__aadClosedProtectedV1 || (globalThis.__aadClosedProtectedV1=new Set());
  const password = el => el.tagName==='INPUT' && (el.type||'').toLowerCase()==='password';
  const fieldProtected = el => secretElements.has(el) || closedSecrets.has(el) || (el.tagName === 'INPUT' && ['password','hidden'].includes((el.type||'').toLowerCase()));
  const protectedNode = el => fieldProtected(el) || (closedRoots.has(el) && closedProtected(el));
  // A file input only ever takes files, through upload; often it is hidden behind a styled button.
  const fileInput = el => el.tagName === 'INPUT' && (el.type||'').toLowerCase() === 'file';
  // Canvas exposes pixels rather than child controls. Its observed element
  // remains the anchor; pointer input still rechecks identity and hit testing.
  const canvas = el => el.tagName === 'CANVAS';
  // Normal DOM traversal cannot enter a closed shadow root. CDP supplies a
  // private root reference; only its observed host becomes a visual surface.
  const closedRoots = new Map();
  const surface = el => canvas(el) || closedRoots.has(el);
  // Inspect protection flags, not field values, and recheck them synchronously
  // with every action and screenshot request.
  const closedProtected = host => {
    const root=closedRoots.get(host);
    if(!root || root.host!==host)return true;
    const stack=[...root.children];
    let scanned=0, protectedFound=false;
    while(stack.length) {
      if(++scanned>20000)return true;
      const el=stack.pop();
      if(fieldProtected(el)){closedSecrets.add(el);protectedFound=true;}
      const inner=closedRoots.get(el)||el.shadowRoot;
      if(inner)stack.push(...inner.children);
      stack.push(...el.children);
    }
    return protectedFound;
  };
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
  const parentsOf = el => {const parents=[];for(let n=el.parentNode||el.host;n;n=n.parentNode||n.host)parents.push(n);return parents;};
  const scopeState = el => {
    const shown=present(el), box=el.getBoundingClientRect();
    const labels=(el.getAttribute('aria-labelledby')||'').split(/\s+/).filter(Boolean).slice(0,8)
      .map(id=>(document.getElementById(id)?.textContent||'').slice(0,256));
    return JSON.stringify([shown,fingerprint(el),labels,box.left+scrollX,box.top+scrollY,box.width,box.height]);
  };
  const scopeCurrent = () => {
    scopeChanges(scopeObserver.takeRecords());
    if (scopeInvalid || !scopeRoot.isConnected) return false;
    const parents=parentsOf(scopeRoot);
    if (parents.length!==scopeParents.length || parents.some((parent,i)=>parent!==scopeParents[i])) return false;
    if (scopeNeedsValidation) {
      const matches=scopeIdentity===null ? [scopeRoot] : document.querySelectorAll('[id="'+CSS.escape(scopeIdentity)+'"]');
      if (matches.length!==1 || matches[0]!==scopeRoot ||
          scopeElements.some(({el,state})=>scopeState(el)!==state)) {
        scopeInvalid=true;
        return false;
      }
      scopeNeedsValidation=false;
    }
    return true;
  };
  const scrollable = el => el===document.body || ((el.scrollHeight>el.clientHeight+1 || el.scrollWidth>el.clientWidth+1) && /(auto|scroll|overlay)/.test(getComputedStyle(el).overflow+getComputedStyle(el).overflowY+getComputedStyle(el).overflowX));
  // The cursor is inherited, so it marks the element where it starts and the
  // items someone picks inside it, not every icon and label within them.
  const PICKED_ROLES = ['listitem','option','menuitem','menuitemcheckbox','menuitemradio','treeitem','row','gridcell','tab'];
  const cursorMarks = (el, cursors) => {
    if (!cursors.test(getComputedStyle(el).cursor)) return false;
    const parent = el.parentElement || (el.parentNode && el.parentNode.host);
    return !parent || !cursors.test(getComputedStyle(parent).cursor) || PICKED_ROLES.includes(roleOf(el));
  };
  // What a person could pick up: HTML5 draggables, items of lists and grids,
  // and whatever shows a grab cursor.
  const draggable = el => el!==document.body && (surface(el) || el.draggable || ['listitem','option','row','treeitem'].includes(roleOf(el)) || cursorMarks(el, /^(grab|grabbing|move)$/));
  // focus() exists on all HTMLElements, including divs that cannot take focus.
  // Explicit negative tabindex still permits programmatic focus.
  const focusable = el => typeof el.focus==='function' && (el.tabIndex>=0 || el.hasAttribute('tabindex') || el.isContentEditable || activeElement()===el);
  const actions = el => {
    if (!enabled(el)) return [];
    if (protectedNode(el)) return password(el) && !el.readOnly ? ['secret_input'] : [];
    if (fileInput(el)) return ['upload'];
    const result=[];
    if (focusable(el)) result.push('focus','key');
    // Delegated listeners (including React) are invisible in our isolated world.
    // The rendered pointer affordance also identifies their clickable elements;
    // check() rechecks this capability and the observed identity before input.
    if ((surface(el) || ['BUTTON','A','INPUT','SELECT'].includes(el.tagName) || el.onclick || roleOf(el)==='button' || checkable(el) || cursorMarks(el, /^pointer$/)) && typeof el.click==='function') result.push('click','hover');
    if (editable(el)) result.push('fill','type_text');
    if (el.tagName==='SELECT' && !el.multiple) result.push('select');
    if (checkable(el)) result.push('check');
    if (surface(el) || scrollable(el)) result.push('scroll');
    if (draggable(el)) result.push('drag');
    if (el===document.body) result.push('navigate','back','forward','reload','open_tab','activate_page','close_page','emulate','clear_site_data');
    return result;
  };
  const check = args => {
    if (scopeRoot && !scopeCurrent()) return {error:'CONTROL.STALE_TARGET'};
    if (args.document_token!==documentToken || args.observation!==observation) return {error:'CONTROL.STALE_TARGET'};
    const found=nodes.get(args.node_id);
    if (!found || !present(found.el) || fingerprint(found.el)!==found.fingerprint) return {error:'CONTROL.STALE_TARGET'};
    if (!enabled(found.el) || (protectedNode(found.el) && args.action!=='secret_input')) return {error:'CONTROL.ACTION_UNSUPPORTED'};
    // A drag's destination is any observed element; everything else needs its action.
    if (args.action!==null && !actions(found.el).includes(args.action)) return {error:'CONTROL.ACTION_UNSUPPORTED'};
    return found;
  };
  globalThis.__aadControlV1 = {
    validate_scope: args => {
      if(args.document_token!==documentToken || args.observation!==observation ||
         !scopeRoot || !scopeCurrent())
        return {error:'CONTROL.STALE_TARGET'};
      return {valid:true};
    },
    // Read-only narrowing may start from an observed node of an incomplete
    // capture. It still checks the retained identity before exporting it to CDP.
    scope_element: args => {
      if (args.document_token!==documentToken || args.observation!==observation ||
          (scopeRoot && !scopeCurrent())) return null;
      const found=nodes.get(args.node_id);
      return found && present(found.el) && fingerprint(found.el)===found.fingerprint ? found.el : null;
    },
    observe: (id,limit,scopeId=null,elementRoot=null) => {
      observation=id; nodes=new Map();
      scopeObserver.disconnect(); scopeRoot=null; scopeInvalid=false; scopeNeedsValidation=false;
      scopeParents=[]; scopeElements=[]; scopeIdentity=elementRoot===null?scopeId:null;
      if (scopeId !== null) {
        const matches=elementRoot===null ? document.querySelectorAll('[id="'+CSS.escape(scopeId)+'"]') : [elementRoot];
        if(matches.length!==1)return {error:matches.length?'CONTROL.AMBIGUOUS':'CONTROL.SCOPE_NOT_FOUND'};
        scopeRoot=matches[0];
        if(!present(scopeRoot))return {error:'CONTROL.STALE_TARGET'};
        scopeParents=parentsOf(scopeRoot);
        scopeObserver.observe(document,{subtree:true,childList:true,attributes:true,characterData:true});
      }
      const items=[];
      // Depth first through the page and every open shadow root: a host's
      // shadow tree first, then its light children, which show only where
      // slotted. Closed shadow roots cannot be entered from script.
      const root=scopeRoot||document.body;
      const stack=root?[root]:[];
      let scanned=0;
      while(stack.length && items.length<=limit && scanned++<20000) {
        const el=stack.pop();
        if(scopeRoot)scopeElements.push({el,state:scopeState(el)});
        if(scopeRoot && el.shadowRoot)scopeObserver.observe(el.shadowRoot,{subtree:true,childList:true,attributes:true,characterData:true});
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
        if (closedRoots.has(el)) node.surface='closed_shadow';
        if (checkable(el)) node.states.checked=checkedOf(el);
        if (fileInput(el)) { node.states.hidden=!visible(el); node.files=Array.from(el.files||[]).slice(0,32).map(f=>f.name.slice(0,256)); node.multiple=!!el.multiple; }
        if (el.tagName==='SELECT') node.options=Array.from(el.options).slice(0,100).map(o=>o.label.trim().slice(0,256));
        return node;
      });
      // Top to bottom, then left to right: the order "the third button" means.
      const order=kept.map((el,index)=>{const box=el.getBoundingClientRect();return [Math.round(box.top+scrollY),Math.round(box.left+scrollX),index];})
        .sort((a,b)=>a[0]-b[0]||a[1]-b[1]||a[2]-b[2]).map(entry=>'n'+entry[2]);
      return {document_token:documentToken,scope_id:scopeId,scope_revision:scopeId===null?null:id,nodes:result,order,truncated:items.length>limit||stack.length>0};
    },
    // The observed elements in node order, for the browser's accessibility tree.
    elements: () => Array.from(nodes.values()).map(found=>found.el),
    reset_closed_roots: () => { closedRoots.clear(); return true; },
    mark_closed_root: root => { if(!root || !root.host || root.host.ownerDocument!==document)return false; closedRoots.set(root.host,root); return true; },
    record_closed_protection: () => { for(const host of closedRoots.keys())closedProtected(host); return true; },
    closed_protected_visible: () => Array.from(closedSecrets).some(visible) || Array.from(closedRoots.keys()).some(host=>visible(host)&&closedProtected(host)),
    surface_state: () => Array.from(nodes,([node_id,{el}])=>({node_id,surface:closedRoots.has(el)?'closed_shadow':null,protected:protectedNode(el),actions:actions(el)})),
    // Rehydrate identities retained by the backend across observation resets
    // and CDP reconnects. Neither identifiers nor this set contain values.
    mark_secret: el => { if(el.ownerDocument===document)secretElements.add(el); return true; },
    secrets_visible: () => Array.from(secretElements).some(visible),
    // Only the dedicated server-owned binding path calls this method. Check
    // identity and replace in one JS turn; no clipboard or keyboard is used.
    secret_input: (args,value) => {
      const found=check(args); if(found.error) return found;
      const el=found.el;
      if(!password(el) || el.readOnly || typeof value!=='string' ||
         Array.from(value).length>1024 || /[\0\r\n]/.test(value)) return {error:'CONTROL.ACTION_UNSUPPORTED'};
      secretElements.add(el);
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(el,value);
      el.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
      el.dispatchEvent(new Event('change',{bubbles:true,composed:true}));
      return {submitted:true,redacted:true};
    },
    // What the page, or one observed region of it, shows as text, with tables
    // as rows of cells. Each part stops at max_characters.
    text: args => {
      if (args.document_token!==documentToken || args.observation!==observation) return {error:'CONTROL.STALE_TARGET'};
      if (scopeRoot && !scopeCurrent()) return {error:'CONTROL.STALE_TARGET'};
      let root=scopeRoot||document.body;
      if (args.node_id) {
        const found=nodes.get(args.node_id);
        if (!found || !found.el.isConnected || fingerprint(found.el)!==found.fingerprint) return {error:'CONTROL.STALE_TARGET'};
        if (scopeRoot && !inside(scopeRoot,found.el)) return {error:'CONTROL.STALE_TARGET'};
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
      // A parent or overlapping element's screenshot could reveal a field
      // whose type the page changed to text. Refuse the entire capture rather
      // than racing a pixel mask against a later layout change.
      for(const secret of [...secretElements,...closedSecrets]) {
        if(!visible(secret))continue;
        const privateBox=secret.getBoundingClientRect();
        if(privateBox.left<box.right && privateBox.right>box.left &&
           privateBox.top<box.bottom && privateBox.bottom>box.top)
          return {error:'CONTROL.ACTION_UNSUPPORTED'};
      }
      for(const host of closedRoots.keys()) {
        if(!visible(host) || !closedProtected(host))continue;
        const privateBox=host.getBoundingClientRect();
        if(privateBox.left<box.right && privateBox.right>box.left &&
           privateBox.top<box.bottom && privateBox.bottom>box.top)
          return {error:'CONTROL.ACTION_UNSUPPORTED'};
      }
      return {x:box.left+scrollX,y:box.top+scrollY,width:box.width,height:box.height};
    },
    // Where trusted pointer input should land, in this frame's CSS pixels. The
    // point must hit the observed element, so nothing laid over it is clicked.
    point: args => {
      const found=check(args); if(found.error) return found;
      const el=found.el;
      if(!args.noscroll) el.scrollIntoView({block:'nearest',inline:'nearest'});
      const box=el.getBoundingClientRect();
      const at=(start,size,f)=>Math.min(start+size-1,Math.max(start+1,start+size*f));
      let x,y;
      if(args.fraction) {
        // A fraction describes the full observed element, including portions
        // clipped by the viewport. Never retarget it to the visible fragment.
        if(box.width<2 || box.height<2)return {error:'CONTROL.OCCLUDED'};
        x=at(box.left,box.width,args.fraction.fx);
        y=at(box.top,box.height,args.fraction.fy);
        if(x<0 || y<0 || x>=innerWidth || y>=innerHeight)return {error:'CONTROL.OCCLUDED'};
      } else {
        // The default still chooses the visible center of a long page body
        // or a partly clipped element, when no particular part was requested.
        const left=Math.max(box.left,0), top=Math.max(box.top,0);
        const width=Math.min(box.right,innerWidth)-left, height=Math.min(box.bottom,innerHeight)-top;
        if(width<2 || height<2)return {error:'CONTROL.OCCLUDED'};
        x=at(left,width,0.5); y=at(top,height,0.5);
      }
      if(!inside(el,deepHit(x,y))) return {error:'CONTROL.OCCLUDED'};
      for(const secret of [...secretElements,...closedSecrets]) {
        if(!visible(secret))continue;
        const privateBox=secret.getBoundingClientRect();
        if(x>=privateBox.left && x<privateBox.right && y>=privateBox.top && y<privateBox.bottom)
          return {error:'CONTROL.ACTION_UNSUPPORTED'};
      }
      for(const host of closedRoots.keys()) {
        if(!visible(host) || !closedProtected(host))continue;
        const privateBox=host.getBoundingClientRect();
        if(x>=privateBox.left && x<privateBox.right && y>=privateBox.top && y<privateBox.bottom)
          return {error:'CONTROL.ACTION_UNSUPPORTED'};
      }
      return {x,y};
    }
  };
  return true;
})()
