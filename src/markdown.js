// Small, safe subset of Markdown for model output. All untrusted text enters
// the DOM via textContent; URLs and raw HTML are intentionally not interpreted.
export function inlineTokens(source) {
  source = String(source);
  const tokens = [];
  const pattern = /(`[^`\n]+`|\*\*[^*\n]+\*\*|\*[^*\n]+\*|__[^_\n]+__|_[^_\n]+_)/g;
  let start = 0;
  for (const match of source.matchAll(pattern)) {
    if (match.index > start) tokens.push({ type: 'text', text: source.slice(start, match.index) });
    const raw = match[0];
    const marker = raw.startsWith('**') || raw.startsWith('__') ? 2 : 1;
    tokens.push({ type: raw.startsWith('`') ? 'code' : marker === 2 ? 'strong' : 'em', text: raw.slice(marker, -marker) });
    start = match.index + raw.length;
  }
  if (start < source.length) tokens.push({ type: 'text', text: source.slice(start) });
  return tokens;
}

function addInline(node, text) {
  for (const token of inlineTokens(text)) {
    const child = token.type === 'text' ? document.createTextNode(token.text) : document.createElement(token.type);
    if (token.type !== 'text') child.textContent = token.text;
    node.appendChild(child);
  }
}

export function renderMarkdown(container, source) {
  const lines = String(source).split('\n');
  const fragment = document.createDocumentFragment();
  let paragraph = [], list = null, code = null;
  const flush = () => {
    if (!paragraph.length) return;
    const p = document.createElement('p');
    paragraph.forEach((line, index) => {
      if (index) p.appendChild(document.createElement('br'));
      addInline(p, line);
    });
    fragment.appendChild(p);
    paragraph = [];
  };
  for (const line of lines) {
    if (/^\s*```/.test(line)) {
      flush(); list = null;
      if (code) { fragment.appendChild(code); code = null; }
      else { code = document.createElement('pre'); code.appendChild(document.createElement('code')); }
    } else if (code) {
      code.firstChild.textContent += (code.firstChild.textContent ? '\n' : '') + line;
    } else if (!line.trim()) {
      flush(); list = null;
    } else if (/^#{1,3} /.test(line)) {
      flush(); list = null;
      const depth = line.match(/^#+/)[0].length;
      const heading = document.createElement(`h${depth + 2}`);
      addInline(heading, line.slice(depth + 1));
      fragment.appendChild(heading);
    } else if (/^\s*[-*] /.test(line)) {
      flush();
      if (!list) { list = document.createElement('ul'); fragment.appendChild(list); }
      const item = document.createElement('li');
      addInline(item, line.replace(/^\s*[-*] /, ''));
      list.appendChild(item);
    } else {
      list = null;
      paragraph.push(line);
    }
  }
  flush();
  if (code) fragment.appendChild(code); // Streaming may not have closed the fence yet.
  container.replaceChildren(fragment);
}
