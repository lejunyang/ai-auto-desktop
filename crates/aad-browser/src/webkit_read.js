// A bounded read of the top document. Never traverse form values, shadow trees or frames.
const [url, outline] = arguments;
if (window !== window.top || location.href !== url || !document.body) return null;
const skip = 'script,style,noscript,template,textarea,input,select,iframe,canvas,object,embed,[hidden],[inert],[aria-hidden="true"],[contenteditable]:not([contenteditable="false"])';
const output = {url, truncated: false};
const nodes = [];
const words = [];
let characters = 0;
let visited = 0;
const stack = [[document.body, null]];
while (stack.length) {
    let [node, label] = stack.pop();
    if (++visited > 20000) { output.truncated = true; break; }
    if (node !== document.body && node.nextSibling) stack.push([node.nextSibling, label]);
    if (node.nodeType === Node.ELEMENT_NODE) {
        if (node.matches(skip)) continue;
        const style = getComputedStyle(node);
        if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse' || style.opacity === '0')
            continue;
        if (outline && node.getClientRects().length) {
            let role = null;
            if (/^H[1-6]$/.test(node.tagName)) role = 'heading';
            else if (node.matches('a[href]')) role = 'link';
            else if (node.matches('button')) role = 'button';
            if (role) {
                if (nodes.length === 200) { output.truncated = true; break; }
                const name = (node.getAttribute('aria-label') || '').slice(0, 256).trim();
                const entry = {role, name};
                nodes.push(entry);
                label = name ? null : entry;
            }
        }
        // Use sibling links to avoid queuing an unbounded number of children.
        if (node.firstChild) stack.push([node.firstChild, label]);
    } else if ((!outline || label) && node.nodeType === Node.TEXT_NODE) {
        const range = document.createRange();
        range.selectNodeContents(node);
        if (range.getBoundingClientRect().width) {
            const remaining = outline ? 256 - label.name.length : 20000 - characters;
            const text = node.substringData(0, remaining).replace(/\s+/g, ' ').trim();
            if (outline) {
                if (text) label.name = (label.name + ' ' + text).trim().slice(0, 256);
            } else if (text) {
                words.push(text);
                characters += text.length + 1;
            }
            if (node.length > remaining || characters >= 20000) {
                output.truncated = true;
                if (!outline) break;
            }
        }
    }
}
if (outline) output.nodes = nodes;
else output.text = words.join(' ');
return output;
