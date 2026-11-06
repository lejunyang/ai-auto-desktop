// Bounded rendered-text naming. Do not import script/style/template source or
// hidden text into a document/container label when no native AX name is used.
const inputRole = el => ({checkbox:'checkbox',radio:'radio',button:'button',submit:'button',reset:'button',range:'slider',file:'button'}[(el.type||'').toLowerCase()] || 'textbox');
const roleOf = el => el.getAttribute('role') || (el.tagName==='INPUT' ? inputRole(el) : ({TEXTAREA:'textbox',BUTTON:'button',A:'link',SELECT:'combobox',BODY:'document',IMG:'img',CANVAS:'canvas',SVG:'img',LI:'listitem',UL:'list',OL:'list',TABLE:'table',TR:'row',TD:'cell',TH:'columnheader',H1:'heading',H2:'heading',H3:'heading',DIALOG:'dialog',FORM:'form',HEADER:'banner',NAV:'navigation',MAIN:'main',ASIDE:'complementary',FOOTER:'contentinfo',SECTION:'region',OUTPUT:'status'}[el.tagName.toUpperCase()] || 'generic'));
const textOf = el => {
  let text='',visited=0;const walker=document.createTreeWalker(el,NodeFilter.SHOW_TEXT);let node;
  while(text.length<256 && visited++<2048 && (node=walker.nextNode())) {
    const parent=node.parentElement;
    if(!parent || parent.closest('script,style,template,noscript,[hidden]') ||
       !parent.getClientRects().length || getComputedStyle(parent).visibility==='hidden')continue;
    text+=(node.nodeValue||'').slice(0,256-text.length);
  }
  return text;
};
const labelOf = el => (el===document.body ? document.title :
  (el.getAttribute('aria-label') || (el.labels && Array.from(el.labels).slice(0,8).map(textOf).join(' ')) ||
  el.getAttribute('alt') || el.getAttribute('title') || el.getAttribute('placeholder') ||
  (['INPUT','TEXTAREA','SELECT'].includes(el.tagName) ? el.id : textOf(el)) || '')).trim().slice(0,256);
