(() => {
  const colorScheme = matchMedia('(prefers-color-scheme: dark)');
  function configureTheme() {
    const dark = colorScheme.matches;
    const ink = dark ? '#edf0f7' : '#30364f';
    const surface = dark ? '#191c27' : '#ffffff';
    const line = dark ? '#929bbb' : '#7d87a5';
    mermaid.initialize({
      startOnLoad: false,
      suppressErrorRendering: true,
      theme: 'base',
      fontFamily: 'Inter, "Noto Sans SC", "PingFang SC", "Segoe UI", sans-serif',
      flowchart: {curve: 'monotoneY', nodeSpacing: 36, rankSpacing: 52, padding: 18},
      sequence: {actorMargin: 48, messageMargin: 36, boxMargin: 12, mirrorActors: false},
      themeVariables: {
        darkMode: dark, background: surface, fontSize: '15px',
        primaryColor: dark ? '#303052' : '#eeeeff',
        primaryBorderColor: dark ? '#9792de' : '#aaa6df', primaryTextColor: ink,
        secondaryColor: dark ? '#203e40' : '#e7f4f0',
        secondaryBorderColor: dark ? '#70afa6' : '#8dbbb0', secondaryTextColor: ink,
        tertiaryColor: dark ? '#252b3b' : '#f4f6fb',
        tertiaryBorderColor: dark ? '#48516b' : '#d4daea', tertiaryTextColor: ink,
        lineColor: line, textColor: ink, edgeLabelBackground: surface,
        clusterBkg: dark ? '#202433' : '#f7f8fc',
        clusterBorder: dark ? '#414960' : '#dfe3ee',
        actorTextColor: ink, actorLineColor: line, signalColor: line, signalTextColor: ink,
        noteBkgColor: dark ? '#453e2b' : '#fff6dc',
        noteBorderColor: dark ? '#a59055' : '#d8c58d', noteTextColor: ink,
        pie1: '#8c87cf', pie2: '#78b8aa', pie3: '#e0bc78',
        pie4: '#86a9d5', pie5: '#c692ae', pie6: '#9eabc1',
        pieStrokeColor: surface, pieStrokeWidth: '2px', pieOpacity: '1'
      },
      themeCSS: '.node rect, .actor { rx: 9px; ry: 9px; } .cluster rect { rx: 12px; ry: 12px; } .nodeLabel, .label, .messageText { font-weight: 500; } .edgePath path, .flowchart-link { stroke-width: 1.5px; }'
    });
  }
  let diagramId = 0;
  let renderQueue = Promise.resolve();

  window.renderMarkdownMermaid = (reset = false) => renderQueue = renderQueue.then(async () => {
    configureTheme();
    const blocks = document.querySelectorAll('article pre > code.language-mermaid');
    for (const code of blocks) {
      const block = code.parentElement;
      if (reset) {
        block.querySelectorAll('.mermaid-diagram, .mermaid-error').forEach(node => node.remove());
        block.classList.remove('mermaid-rendered');
        delete block.dataset.mermaid;
      }
      if (!block.isConnected || block.dataset.mermaid) continue;
      block.dataset.mermaid = 'rendering';
      const diagram = document.createElement('div');
      diagram.className = 'mermaid-diagram';
      block.append(diagram);
      try {
        const {svg, bindFunctions} = await mermaid.render(
          `markdown-mermaid-${++diagramId}`, code.textContent, diagram
        );
        if (!block.isConnected) continue;
        diagram.innerHTML = svg;
        bindFunctions?.(diagram);
        block.classList.add('mermaid-rendered');
        block.dataset.mermaid = 'rendered';
      } catch (error) {
        if (!block.isConnected) continue;
        diagram.className = 'mermaid-error';
        diagram.textContent = `Mermaid 图表渲染失败：${error.message || error}`;
        block.dataset.mermaid = 'error';
      }
    }
  });

  colorScheme.addEventListener('change', () => window.renderMarkdownMermaid(true));
  window.renderMarkdownMermaid();
})();
