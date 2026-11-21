(() => {
  if(globalThis.__aadBidiV1)return true;
  const protectedElements=new Set();
  const frameElements=new Set();
  const visible=el=>el.isConnected && el.ownerDocument===document &&
    el.getClientRects().length>0 && getComputedStyle(el).visibility!=='hidden';
  const overlaps=(a,b)=>a.left<b.right&&a.right>b.left&&a.top<b.bottom&&a.bottom>b.top;
  // Only the current observation can supply a frame route. Keep DOM references
  // in the private realm; the caller receives only a supported/unsupported flag.
  let frameScene;
  function viewport(win) {
    const v=win.visualViewport;
    return v && v.scale===1 && v.offsetLeft===0 && v.offsetTop===0;
  }
  function embedding(win) {
    const frame=win.frameElement, parent=win.parent, doc=parent.document;
    if(!frame || frame.localName!=='iframe' || frame.getRootNode()!==doc ||
       frame.contentDocument!==win.document || frame.contentWindow!==win ||
       frame.getClientRects().length!==1 || !viewport(win)) return null;
    const parents=[];
    for(let el=frame;el;el=el.parentElement) {
      const s=parent.getComputedStyle(el);
      if(parents.length>=128 || el.namespaceURI!=='http://www.w3.org/1999/xhtml' ||
         el.assignedSlot || el.shadowRoot || el.inert || s.visibility!=='visible' ||
         s.opacity!=='1' || s.transform!=='none' || s.perspective!=='none' ||
         s.scale!=='none' || s.rotate!=='none' || s.translate!=='none' ||
         s.offsetPath!=='none' || !['1','normal'].includes(s.zoom) ||
         s.filter!=='none' || s.contentVisibility==='hidden') return null;
      parents.push(el);
    }
    const s=parent.getComputedStyle(frame), box=frame.getBoundingClientRect();
    const inset=side=>parseFloat(s['border'+side+'Width'])+parseFloat(s['padding'+side]);
    const x=box.left+inset('Left'), y=box.top+inset('Top');
    if(!Number.isSafeInteger(x) || !Number.isSafeInteger(y) ||
       box.right-inset('Right')!==x+win.innerWidth || box.bottom-inset('Bottom')!==y+win.innerHeight)
      return null;
    return {frame,doc,parents,x,y};
  }
  function bindFrames(observation,urls) {
    frameScene=null;
    try {
      const links=[];
      let win=window;
      for(const url of urls) {
        if(!/^https?:$/.test(win.location.protocol) || win.location.origin!==location.origin ||
           win.location.href!==url || !viewport(win)) return false;
        const link=win===top ? null : embedding(win);
        if(win!==top && !link) return false;
        links.push({win,document:win.document,url,width:win.innerWidth,height:win.innerHeight,link});
        if(win===top) break;
        win=win.parent;
      }
      if(!links.length || links.length!==urls.length || links.at(-1).win!==top ||
         links.reduce((n,l)=>n+(l.link?.parents.length||0),0)>256) return false;
      frameScene={observation,links};
      return true;
    } catch { return false; }
  }
  function framePoint(args,x,y) {
    if(window===top) return {x,y};
    if(!frameScene || frameScene.observation!==args.observation) return {error:'CONTROL.ACTION_UNSUPPORTED'};
    try {
      for(const layer of frameScene.links) {
        const {win,link}=layer;
        if(win.document!==layer.document || win.location.href!==layer.url ||
           win.innerWidth!==layer.width || win.innerHeight!==layer.height || !viewport(win))
          return {error:'CONTROL.STALE_TARGET'};
        if(win.document.visibilityState!=='visible' || x<0 || y<0 || x>=layer.width || y>=layer.height)
          return {error:'CONTROL.OCCLUDED'};
        if(!link) return win.document.hasFocus() ? {x,y} : {error:'CONTROL.FOCUS_FAILED'};
        const now=embedding(win);
        if(!now || now.frame!==link.frame || now.doc!==link.doc || now.x!==link.x || now.y!==link.y ||
           now.parents.length!==link.parents.length || now.parents.some((p,i)=>p!==link.parents[i]))
          return {error:'CONTROL.STALE_TARGET'};
        x+=now.x; y+=now.y;
        if(now.doc.elementFromPoint(x,y)!==now.frame) return {error:'CONTROL.OCCLUDED'};
        if(args.requireFocus && now.doc.activeElement!==now.frame) return {error:'CONTROL.FOCUS_FAILED'};
      }
    } catch { /* A navigation can revoke same-origin access mid-request. */ }
    return {error:'CONTROL.STALE_TARGET'};
  }
  // Navigation.entries is the contiguous, same-origin segment around the
  // current entry, not a filtered list that skips intervening foreign origins.
  // Read only browser-owned identity/URLs; never import application state.
  const historyTarget=delta=>{
    const nav=globalThis.navigation,current=nav?.currentEntry;
    if(!current || !Number.isInteger(current.index) || current.index<0)return null;
    const entries=nav.entries();if(entries.length>4096)return null;
    const target=entries[current.index+delta];
    if(!target?.url || !current.url || !/^https?:$/.test(location.protocol))return null;
    if(new URL(target.url).origin!==location.origin)return null;
    const identity=entry=>({id:entry.id,key:entry.key,url:entry.url});
    return {source:identity(current),destination:identity(target)};
  };
  globalThis.__aadBidiV1={
    bind_frames: bindFrames,
    // A slot in a closed shadow tree can transform a light-DOM iframe while
    // assignedSlot is null. BiDi can inspect these exact hosts for closed roots.
    embedding_hosts: observation=>frameScene?.observation===observation ?
      frameScene.links.flatMap(l=>l.link?.parents||[]) : [],
    focus_for_keys: args=>{
      if(window!==top) {
        const p=globalThis.__aadBidiV1.point(args);
        if(p.error)return p;
      }
      return globalThis.__aadControlV1.act(args);
    },
    verify_focus: args=>{
      if(window!==top) {
        const p=globalThis.__aadBidiV1.point({...args,requireFocus:true});
        if(p.error)return p;
      }
      return globalThis.__aadControlV1.verify(args);
    },
    history_targets: ()=>({back:historyTarget(-1),forward:historyTarget(1)}),
    history: (args,expected)=>{
      const confirmed=globalThis.__aadControlV1.confirm(args);if(confirmed.error)return confirmed;
      const current=historyTarget(args.action==='back'?-1:1);
      if(!current)return {error:'CONTROL.ACTION_UNSUPPORTED'};
      if(['source','destination'].some(which=>['id','key','url'].some(field=>current[which][field]!==expected?.[which]?.[field])))
        return {error:'CONTROL.STALE_TARGET'};
      // The key is pinned after URL authorization, and its id/URL is compared
      // in the same JS turn as traversal. Do not issue an unchecked delta.
      globalThis.__aadBidiHistoryFailed=false;
      const result=navigation.traverseTo(current.destination.key);
      result.committed.catch(()=>{globalThis.__aadBidiHistoryFailed=true});
      result.finished.catch(()=>{});
      return {submitted:true};
    },
    point: args=>{
      // A child action never scrolls an unverified embedding element into view.
      const point=globalThis.__aadControlV1.point(window===top?args:{...args,noscroll:true});if(point.error)return point;
      const el=globalThis.__aadControlV1.scope_element(args);if(!el)return {error:'CONTROL.STALE_TARGET'};
      const x=Math.floor(point.x),y=Math.floor(point.y);
      let hit=document.elementFromPoint(x,y);
      while(hit?.shadowRoot){const inner=hit.shadowRoot.elementFromPoint(x,y);if(!inner||inner===hit)break;hit=inner;}
      let inside=false;for(let n=hit;n;n=n.parentNode||n.host)if(n===el){inside=true;break;}
      if(!inside)return {error:'CONTROL.OCCLUDED'};
      if(window!==top) {
        // A hit on an embedding node would send physical input into another
        // document, even if that node is inside the observed control.
        for(const frame of frameElements)if(visible(frame)){
          const r=frame.getBoundingClientRect();
          if(x>=r.left&&x<r.right&&y>=r.top&&y<r.bottom)return {error:'CONTROL.ACTION_UNSUPPORTED'};
        }
      }
      for(const field of protectedElements)if(visible(field)){
        const r=field.getBoundingClientRect();if(x>=r.left&&x<r.right&&y>=r.top&&y<r.bottom)return {error:'CONTROL.ACTION_UNSUPPORTED'};
      }
      return framePoint(args,x,y);
    },
    // Returned nodes are private BiDi shared references, never public output.
    // Every password identity stays protected until this document is destroyed.
    scan: (root=document)=>{
      const stack=[...root.children],result=[];
      while(stack.length) {
        if(result.length>=20000)throw Error('bounded protection scan exceeded');
        const el=stack.pop();result.push(el);
        if(['IFRAME','FRAME'].includes(el.tagName) ||
           (window!==top && ['OBJECT','EMBED'].includes(el.tagName)))frameElements.add(el);
        if(el.tagName==='INPUT' && ['password','hidden'].includes(el.type)) {
          protectedElements.add(el);globalThis.__aadControlV1.mark_secret(el);
        }
        if(el.shadowRoot)stack.push(...el.shadowRoot.children);
        stack.push(...el.children);
      }
      return result;
    },
    // Capture performs no scrolling. Offscreen elements need an explicit,
    // anchored input action first. A child frame's pixels are not yet admitted.
    capture: args=>{
      const el=globalThis.__aadControlV1.scope_element(args);
      if(!el || !visible(el))return {error:'CONTROL.STALE_TARGET'};
      const box=el.getBoundingClientRect();
      if(box.width<1||box.height<1||box.left<0||box.top<0||box.right>innerWidth||box.bottom>innerHeight)
        return {error:'CONTROL.OCCLUDED'};
      for(const field of protectedElements)if(visible(field)&&overlaps(box,field.getBoundingClientRect()))
        return {error:'CONTROL.ACTION_UNSUPPORTED'};
      for(const frame of frameElements)if(visible(frame)&&overlaps(box,frame.getBoundingClientRect()))
        return {error:'CONTROL.ACTION_UNSUPPORTED'};
      return {ok:true,width:box.width,height:box.height};
    }
  };
  return true;
})()
