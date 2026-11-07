(() => {
  if(globalThis.__aadBidiV1)return true;
  const protectedElements=new Set();
  const frameElements=new Set();
  const visible=el=>el.isConnected && el.ownerDocument===document &&
    el.getClientRects().length>0 && getComputedStyle(el).visibility!=='hidden';
  const overlaps=(a,b)=>a.left<b.right&&a.right>b.left&&a.top<b.bottom&&a.bottom>b.top;
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
      const point=globalThis.__aadControlV1.point(args);if(point.error)return point;
      const el=globalThis.__aadControlV1.scope_element(args);if(!el)return {error:'CONTROL.STALE_TARGET'};
      const x=Math.floor(point.x),y=Math.floor(point.y);
      let hit=document.elementFromPoint(x,y);
      while(hit?.shadowRoot){const inner=hit.shadowRoot.elementFromPoint(x,y);if(!inner||inner===hit)break;hit=inner;}
      let inside=false;for(let n=hit;n;n=n.parentNode||n.host)if(n===el){inside=true;break;}
      if(!inside)return {error:'CONTROL.OCCLUDED'};
      for(const field of protectedElements)if(visible(field)){
        const r=field.getBoundingClientRect();if(x>=r.left&&x<r.right&&y>=r.top&&y<r.bottom)return {error:'CONTROL.ACTION_UNSUPPORTED'};
      }
      return {x,y};
    },
    // Returned nodes are private BiDi shared references, never public output.
    // Every password identity stays protected until this document is destroyed.
    scan: (root=document)=>{
      const stack=[...root.children],result=[];
      while(stack.length) {
        if(result.length>=20000)throw Error('bounded protection scan exceeded');
        const el=stack.pop();result.push(el);
        if(['IFRAME','FRAME'].includes(el.tagName))frameElements.add(el);
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
