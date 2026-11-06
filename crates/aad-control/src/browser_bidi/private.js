(() => {
  if(globalThis.__aadBidiV1)return true;
  const protectedElements=new Set();
  const frameElements=new Set();
  const visible=el=>el.isConnected && el.ownerDocument===document &&
    el.getClientRects().length>0 && getComputedStyle(el).visibility!=='hidden';
  const overlaps=(a,b)=>a.left<b.right&&a.right>b.left&&a.top<b.bottom&&a.bottom>b.top;
  globalThis.__aadBidiV1={
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
