const { test, expect } = require('@playwright/test');
const { openGalleryImage } = require('./helpers');

test('folder stars update favourites without reloading the directory', async ({page}) => {
  await page.goto('/');
  await expect(page.locator('#directory-favourite-toggle')).toHaveCount(0);
  await page.evaluate(() => { window.directoryFavouriteTestMarker = true; });
  const star = page.locator('[data-directory-favourite-toggle="/shortcut-folder/"]');
  await star.click();
  await expect(star).toHaveText('★');
  await expect(page.locator('.directory-favourites')).toBeVisible();
  await expect(page.locator('.directory-favourite a')).toHaveText('shortcut-folder');
  expect(await page.evaluate(() => window.directoryFavouriteTestMarker)).toBe(true);
  await star.click();
  await expect(star).toHaveText('☆');
  await expect(page.locator('.directory-favourites')).toHaveCount(0);

  for (const suffix of ['', '-2', '-3', '-4']) {
    await page.locator(`[data-directory-favourite-toggle="/shortcut-folder${suffix}/"]`).click();
  }
  await page.reload();
  await expect(page.locator('.directory-favourites-more')).toBeVisible();
  await expect(page.locator('.directory-favourites-more summary')).toContainText('更多');
  await expect(page.locator('.directory-favourites-menu .directory-favourite')).toHaveCount(1);
  await page.locator('.directory-favourites-more summary').click();
  await expect(page.locator('.directory-favourites-more')).toHaveAttribute('open', '');
  await page.locator('.directory-favourites-menu [data-directory-favourite-remove="/shortcut-folder-4/"]').click();
  await expect(page.locator('.directory-favourites')).toBeVisible();
  await expect(page.locator('.directory-favourites-list .directory-favourite')).toHaveCount(3);
});

test('portrait layout, horizontal transitions, toolbar wake-up, and explicit close', async ({page}) => {
  await openGalleryImage(page);
  await expect.poll(() => page.locator('.lightbox-image').evaluate(image => image.naturalWidth)).toBe(600);

  const layout = await page.evaluate(() => {
    const stage = document.querySelector('.lightbox-stage').getBoundingClientRect();
    const image = document.querySelector('.lightbox-image').getBoundingClientRect();
    return {stage, image};
  });
  expect(Math.abs(layout.image.top - layout.stage.top)).toBeLessThanOrEqual(1);
  expect(Math.abs(layout.image.left - layout.stage.left)).toBeLessThanOrEqual(1);
  expect(Math.abs(layout.image.width - layout.stage.width)).toBeLessThanOrEqual(1);
  expect(Math.abs(layout.image.height - layout.stage.height)).toBeLessThanOrEqual(1);
  await expect(page.locator('#image-lightbox')).toHaveClass(/chrome-hidden/);
  await expect(page.locator('.lightbox-filmstrip')).toBeVisible();
  await expect(page.locator('.lightbox-close')).toBeVisible();
  await expect(page.locator('.lightbox-position')).toHaveText('1 / 3');
  await expect(page.locator('.filmstrip-slot')).toHaveCount(7);
  await expect(page.locator('.filmstrip-slot').nth(3).locator('.filmstrip-thumb')).toHaveAttribute('aria-current', 'true');
  const singleViewerSurface = await page.locator('#image-lightbox').evaluate(element => {
    const style = getComputedStyle(element);
    const backdrop = getComputedStyle(element.querySelector('.carousel-backdrop'));
    return {
      background: style.backgroundColor,
      backdropFilter: style.backdropFilter || style.webkitBackdropFilter,
      imageBackdropDisplay: backdrop.display
    };
  });
  expect(singleViewerSurface.background).not.toBe('rgba(0, 0, 0, 0)');
  expect(singleViewerSurface.backdropFilter).toContain('blur(');
  expect(singleViewerSurface.imageBackdropDisplay).toBe('none');

  const pathBeforeKey = await page.locator('#image-lightbox').getAttribute('data-file-path');
  await page.keyboard.press('ArrowDown');
  await expect(page.locator('#image-lightbox')).not.toHaveAttribute('data-file-path', pathBeforeKey);
  const pathAfterArrowDown = await page.locator('#image-lightbox').getAttribute('data-file-path');
  await page.keyboard.press('j');
  await expect(page.locator('#image-lightbox')).toHaveAttribute('data-file-path', pathBeforeKey);
  await page.keyboard.press('k');
  await expect(page.locator('#image-lightbox')).toHaveAttribute('data-file-path', pathAfterArrowDown);
  await expect(page.locator('#image-lightbox')).toHaveClass(/chrome-hidden/);
  const transition = await page.evaluate(() => {
    const outgoing = document.querySelector('.preview-outgoing');
    return outgoing?.getAnimations()[0]?.effect.getKeyframes().map(frame => frame.transform) || [];
  });
  expect(transition.length).toBeGreaterThanOrEqual(2);
  expect(transition.join(' ')).toContain('12%');
  expect(transition.join(' ')).toMatch(/translate3d\([^,]+%,\s*0/);

  await page.mouse.move(720, 898);
  await expect(page.locator('#image-lightbox')).not.toHaveClass(/chrome-hidden/);
  await page.waitForTimeout(2700);
  await expect(page.locator('#image-lightbox')).not.toHaveClass(/chrome-hidden/);
  await page.mouse.move(720, 180);
  await page.waitForTimeout(2700);
  await expect(page.locator('#image-lightbox')).toHaveClass(/chrome-hidden/);

  await page.locator('.lightbox-image').click({position: {x: 300, y: 300}});
  await expect(page.locator('#image-lightbox')).toBeVisible();
  await expect(page.locator('#image-lightbox')).not.toHaveClass(/chrome-hidden/);
  await page.mouse.click(5, 450);
  await expect(page.locator('#image-lightbox')).toBeVisible();
  await page.locator('.lightbox-close').click();
  await expect(page.locator('#image-lightbox')).toBeHidden();
});

test('secondary viewer tools stay hidden until More is opened', async ({page}) => {
  await openGalleryImage(page);
  await expect(page.locator('.viewer-extras')).toBeHidden();
  await page.locator('.lightbox-image').click({position: {x: 300, y: 300}});
  await expect(page.locator('.viewer-more-toggle')).toBeVisible();
  await expect(page.locator('.preview-step')).toHaveCount(2);
  await expect(page.locator('.viewer-extras')).toBeHidden();

  await page.locator('.viewer-more-toggle').click();
  await expect(page.locator('.viewer-more-toggle')).toHaveAttribute('aria-expanded', 'true');
  await expect(page.locator('.viewer-extras')).toBeVisible();
  await expect(page.locator('[data-view="fit"]')).toBeVisible();
  await page.locator('[data-zoom="in"]').click();
  await expect.poll(() => page.locator('.lightbox-image').evaluate(image => image.style.transform)).toContain('scale(1.25)');
  await page.locator('[data-view="fit"]').click();
  await expect.poll(() => page.locator('.lightbox-image').evaluate(image => image.style.transform)).toContain('scale(1)');
  await page.locator('[data-info]').click();
  await expect(page.locator('.image-info')).toBeVisible();
  await page.locator('[data-info]').click();
  await expect(page.locator('.image-info')).toBeHidden();
  await page.locator('.shortcut-help-toggle').click();
  await expect(page.locator('.shortcut-help')).toBeVisible();
  await page.locator('.shortcut-help-toggle').click();
  await expect(page.locator('.shortcut-help')).toBeHidden();

  await page.keyboard.press('Escape');
  await expect(page.locator('.viewer-extras')).toBeHidden();
  await expect(page.locator('#image-lightbox')).toBeVisible();
});

test('hidden viewer controls expose live favourite and deletion state on the image', async ({page}) => {
  await page.request.delete('/01-portrait.svg?mode=favourite');
  await page.request.delete('/01-portrait.svg?mode=deletion-mark');
  await openGalleryImage(page);
  const state = page.locator('.lightbox-state');
  const heart = state.locator('.lightbox-favourite-state');
  const deletion = state.locator('.lightbox-deletion-state');

  await expect(state).toHaveCSS('opacity', '1');
  await expect(heart).not.toHaveClass(/liked/);
  await expect(heart).toBeHidden();
  await expect(deletion).toBeHidden();
  await expect.poll(() => page.locator('.lightbox-image').evaluate(image => image.naturalWidth)).toBe(600);
  const placement = await page.evaluate(() => {
    const stage = document.querySelector('.lightbox-stage').getBoundingClientRect();
    const image = document.querySelector('.lightbox-image');
    const state = document.querySelector('.lightbox-state').getBoundingClientRect();
    const scale = Math.min(1, stage.width / image.naturalWidth, stage.height / image.naturalHeight);
    const imageRight = stage.left + (stage.width + image.naturalWidth * scale) / 2;
    const imageBottom = stage.top + (stage.height + image.naturalHeight * scale) / 2;
    return {rightInset: imageRight - state.right, bottomInset: imageBottom - state.bottom};
  });
  expect(placement.rightInset).toBeGreaterThanOrEqual(8);
  expect(placement.rightInset).toBeLessThanOrEqual(16);
  expect(placement.bottomInset).toBeGreaterThanOrEqual(8);
  expect(placement.bottomInset).toBeLessThanOrEqual(16);

  await page.keyboard.press('f');
  await expect(page.locator('#favourite-toggle')).toHaveAttribute('aria-pressed', 'true');
  await expect(heart).toHaveClass(/liked/);
  await expect(heart).toBeVisible();
  await page.keyboard.press('d');
  await expect(page.locator('#deletion-toggle')).toHaveAttribute('aria-pressed', 'true');
  await expect(deletion).toBeVisible();
  await expect(state).toHaveCSS('opacity', '1');

  await page.locator('.lightbox-image').click({position: {x: 300, y: 300}});
  await expect(state).toHaveCSS('opacity', '1');
  await page.waitForTimeout(2700);
  await expect(state).toHaveCSS('opacity', '1');

  await page.request.delete('/01-portrait.svg?mode=favourite');
  await page.request.delete('/01-portrait.svg?mode=deletion-mark');
});

test('d toggles the deletion mark without a dialog while Ctrl+K deletes directly', async ({page}) => {
  await page.request.delete('/03-square.svg?mode=deletion-mark');
  let directDeletes = 0;
  await page.route('**/03-square.svg*', async route => {
    const request = route.request();
    const url = new URL(request.url());
    if (request.method() === 'DELETE' && !url.searchParams.has('mode')) {
      directDeletes += 1;
      await route.fulfill({status: 204});
      return;
    }
    await route.continue();
  });
  await openGalleryImage(page, '03-square.svg');

  await page.keyboard.press('d');
  await expect(page.locator('#deletion-toggle')).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('.lightbox-deletion-state')).toBeVisible();
  await expect(page.locator('#delete-dialog')).toBeHidden();
  await page.waitForTimeout(450);
  await expect(page.locator('#image-lightbox')).toBeVisible();
  expect(directDeletes).toBe(0);

  await page.keyboard.press('Shift+d');
  await expect(page.locator('#deletion-toggle')).toHaveAttribute('aria-pressed', 'false');
  expect(directDeletes).toBe(0);

  await page.keyboard.press('d');
  await page.keyboard.press('d');
  await page.waitForTimeout(100);
  expect(directDeletes).toBe(0);

  const deletedPath = await page.locator('#image-lightbox').getAttribute('data-file-path');
  await page.keyboard.press('Control+k');
  await expect.poll(() => directDeletes).toBe(1);
  await expect(page.locator('#delete-dialog')).toBeHidden();
  await expect(page.locator('#image-lightbox')).not.toHaveAttribute('data-file-path', deletedPath);
  await page.request.delete('/03-square.svg?mode=deletion-mark');
});

test('marked-image view swaps organising actions and exposes batch actions', async ({page}) => {
  await page.request.put('/01-portrait.svg?mode=favourite');
  await page.request.put('/02-landscape.svg?mode=deletion-mark');
  await page.goto('/?view=gallery');

  const moveFavourite = page.locator('[data-move-images="favourites"]');
  await expect(moveFavourite).toBeVisible();
  await moveFavourite.click();
  await expect(page.locator('.move-description')).toHaveText('将 1 张已点赞图片移入 favourites/。同名文件会自动重命名。');
  await page.locator('[data-move-cancel]').click();
  await expect(page.locator('#delete-marked-images')).toBeHidden();
  await expect(page.locator('#clear-deletion-marks')).toBeHidden();

  await page.locator('#deletion-filter').click();
  await expect(page.locator('#deletion-filter')).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('#deletion-filter-label')).toHaveText('查看全部图片');
  await expect(page.locator('[data-move-images="favourites"]')).toBeHidden();
  await expect(page.locator('#delete-marked-images')).toBeVisible();
  await expect(page.locator('#clear-deletion-marks')).toBeVisible();
  await expect(page.locator('.entry.image:visible')).toHaveCount(1);

  await page.locator('#delete-marked-images').click();
  await expect(page.locator('#delete-marked-dialog')).toBeVisible();
  await page.locator('#delete-marked-cancel').click();
  await page.locator('#deletion-filter').click();
  await expect(page.locator('#deletion-filter-label')).toHaveText('查看待删除列表');
  await expect(page.locator('[data-move-images="favourites"]')).toBeVisible();
  await expect(page.locator('#delete-marked-images')).toBeHidden();
  await expect(page.locator('#clear-deletion-marks')).toBeHidden();

  await page.request.delete('/02-landscape.svg?mode=deletion-mark');
  await page.request.delete('/01-portrait.svg?mode=favourite');
});

test('favourites organiser reports when there are no images to move', async ({page}) => {
  await Promise.all(['01-portrait.svg', '02-landscape.svg', '03-square.svg'].map(name =>
    page.request.delete(`/${name}?mode=favourite`)
  ));
  await page.goto('/?view=gallery');

  await page.locator('[data-move-images="favourites"]').click();
  await expect(page.locator('#directory-notice')).toHaveText('没有图片需要移动');
  await expect(page.locator('#directory-notice')).toBeVisible();
  await expect(page.locator('.move-dialog')).toBeHidden();
});

test('comments own keyboard input and reuse the Markdown review identity', async ({page}) => {
  await page.addInitScript(() => localStorage.setItem('webdir-review-identity', 'Alice'));
  await openGalleryImage(page);
  await page.request.post('/01-portrait.svg?mode=gallery-comments', {data: {type: 'delete-all'}});
  await page.locator('.lightbox-image').click({position: {x: 300, y: 300}});
  await page.locator('.comment-toggle').click();

  await expect(page.locator('[data-comment-author]')).toBeHidden();
  await expect(page.locator('.gallery-comment-identity strong')).toHaveText('Alice');
  const pathBeforeTyping = await page.locator('#image-lightbox').getAttribute('data-file-path');
  const editor = page.locator('.gallery-comment-composer textarea');
  await expect(editor).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(editor).not.toBeFocused();
  await expect(page.locator('.gallery-comments')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.locator('.gallery-comments')).toBeHidden();
  await page.keyboard.press('c');
  await expect(editor).toBeFocused();
  await page.keyboard.type('jkfmp');
  await editor.press('Meta+Enter');
  await expect(editor).toHaveValue('jkfmp\n');
  await expect(page.locator('#image-lightbox')).toHaveAttribute('data-file-path', pathBeforeTyping);
  await editor.press('Enter');
  await expect(page.locator('.gallery-comment-list article')).toHaveCount(1);

  await page.locator('[data-comment-edit]').click();
  await editor.fill('编辑后仍可输入 j 和 k');
  await editor.press('Enter');
  await expect(page.locator('.gallery-comment-list article p')).toHaveText('编辑后仍可输入 j 和 k');
  await expect(page.locator('#image-lightbox')).toHaveAttribute('data-file-path', pathBeforeTyping);

  await page.locator('[data-comment-delete-all]').click();
  await expect(page.locator('[data-comment-delete-all]')).toHaveText('确认全部删除');
  await page.locator('[data-comment-delete-all]').click();
  await expect(page.locator('.gallery-comment-list article')).toHaveCount(0);
  await expect(page.locator('[data-comment-delete-all]')).toBeHidden();
});

test('comments keep a single-column drawer in a wide browser window', async ({page}) => {
  await openGalleryImage(page);
  await page.keyboard.press('c');

  const layout = await page.locator('.gallery-comments').evaluate(element => {
    const drawer = element.getBoundingClientRect();
    const header = element.querySelector(':scope > header').getBoundingClientRect();
    const image = element.querySelector('.gallery-comments-image').getBoundingClientRect();
    return {
      columns: getComputedStyle(element).gridTemplateColumns.trim().split(/\s+/).length,
      drawer: {left: drawer.left, right: drawer.right},
      header: {left: header.left, right: header.right},
      image: {left: image.left, right: image.right}
    };
  });
  expect(layout.columns).toBe(1);
  expect(Math.abs(layout.header.left - layout.drawer.left)).toBeLessThanOrEqual(1);
  expect(Math.abs(layout.header.right - layout.drawer.right)).toBeLessThanOrEqual(1);
  expect(Math.abs(layout.image.left - layout.drawer.left)).toBeLessThanOrEqual(1);
  expect(Math.abs(layout.image.right - layout.drawer.right)).toBeLessThanOrEqual(1);
});

test('submitting a comment scrolls to the latest comment', async ({page}) => {
  await page.addInitScript(() => localStorage.setItem('webdir-review-identity', 'Alice'));
  await page.request.post('/01-portrait.svg?mode=gallery-comments', {data: {type: 'delete-all'}});
  for (let index = 0; index < 8; index += 1) {
    await page.request.post('/01-portrait.svg?mode=gallery-comments', {data: {
      type: 'add',
      comment: {
        id: `existing-${index}`,
        image: '01-portrait.svg',
        author: 'Alice',
        body: `已有评论 ${index + 1}`,
        created_at: new Date(2026, 8, 9, 10, index).toISOString()
      }
    }});
  }
  await openGalleryImage(page);
  await page.keyboard.press('c');

  const list = page.locator('.gallery-comment-list');
  const editor = page.locator('.gallery-comment-composer textarea');
  await expect(list.locator('article')).toHaveCount(8);
  await expect.poll(() => list.evaluate(element =>
    element.scrollHeight - element.scrollTop - element.clientHeight
  )).toBeLessThanOrEqual(1);
  await list.evaluate(element => { element.scrollTop = 0; });
  await editor.fill('刚刚添加的最新评论');
  await editor.press('Enter');
  await expect(list.locator('article')).toHaveCount(9);
  await expect(list.locator('article').last().locator('p')).toHaveText('刚刚添加的最新评论');
  await expect.poll(() => list.evaluate(element =>
    element.scrollHeight - element.scrollTop - element.clientHeight
  )).toBeLessThanOrEqual(1);
});

test('comments keep the editor reachable in a compact browser window', async ({page}) => {
  await page.setViewportSize({width: 560, height: 360});
  await page.addInitScript(() => localStorage.removeItem('webdir-review-identity'));
  await page.request.post('/01-portrait.svg?mode=gallery-comments', {data: {type: 'delete-all'}});
  await openGalleryImage(page);
  await page.keyboard.press('c');

  const drawer = page.locator('.gallery-comments');
  const composer = page.locator('.gallery-comment-composer');
  const author = composer.locator('input[name="author"]');
  const editor = composer.locator('textarea[name="body"]');
  await expect(drawer).toBeVisible();
  await drawer.evaluate(element => Promise.all(
    element.getAnimations().map(animation => animation.finished)
  ));
  await expect(author).toBeFocused();
  await expect(author).toBeInViewport();
  await expect(editor).toBeInViewport();

  const layout = await drawer.evaluate(element => {
    const composer = element.querySelector('.gallery-comment-composer');
    const bounds = element.getBoundingClientRect();
    return {
      top: bounds.top,
      bottom: bounds.bottom,
      viewportHeight: innerHeight,
      composerScrollable: composer.scrollHeight > composer.clientHeight
    };
  });
  expect(layout.top).toBeGreaterThanOrEqual(0);
  expect(layout.bottom).toBeLessThanOrEqual(layout.viewportHeight);
  expect(layout.composerScrollable).toBe(true);

  await composer.evaluate(element => { element.scrollTop = element.scrollHeight; });
  await expect(composer.locator('.comment-submit')).toBeInViewport();
});

test('slideshow fills the viewport and favourite particles are not stage-clipped', async ({page}) => {
  await openGalleryImage(page);
  await page.locator('.lightbox-image').click({position: {x: 300, y: 300}});
  await page.evaluate(() => {
    const lightbox = document.querySelector('#image-lightbox');
    window.__webkitFullscreenCalls = 0;
    Object.defineProperty(lightbox, 'requestFullscreen', {value: undefined, configurable: true});
    Object.defineProperty(lightbox, 'webkitRequestFullscreen', {
      value: () => { window.__webkitFullscreenCalls += 1; },
      configurable: true
    });
  });
  await page.locator('#carousel-toggle').click();
  await expect(page.locator('#image-lightbox')).toHaveClass(/carousel-mode/);
  await expect.poll(() => page.evaluate(() => window.__webkitFullscreenCalls)).toBe(1);
  await expect(page.locator('.lightbox-close')).toBeHidden();
  await expect(page.locator('.lightbox-position')).toBeHidden();
  await expect(page.locator('.lightbox-filmstrip')).toBeHidden();
  await expect(page.locator('#image-lightbox figcaption')).toBeHidden();
  await expect(page.locator('.carousel-hud')).toBeVisible();
  await expect(page.locator('.carousel-backdrop')).toBeVisible();
  const imageBackdrop = await page.locator('.carousel-backdrop').evaluate(element => {
    const style = getComputedStyle(element);
    return {backgroundImage: style.backgroundImage, filter: style.filter, opacity: style.opacity};
  });
  expect(imageBackdrop.backgroundImage).not.toBe('none');
  expect(imageBackdrop.filter).toContain('blur(12px)');
  expect(Number(imageBackdrop.opacity)).toBeGreaterThan(0);
  const fullscreen = await page.locator('#image-lightbox').evaluate(element => {
    const box = element.getBoundingClientRect();
    const stage = element.querySelector('.lightbox-stage').getBoundingClientRect();
    return {box: [box.width, box.height], stage: [stage.width, stage.height], viewport: [innerWidth, innerHeight]};
  });
  expect(fullscreen.box).toEqual(fullscreen.viewport);
  expect(fullscreen.stage[1]).toBeGreaterThanOrEqual(fullscreen.viewport[1] - 1);
  await page.locator('[data-carousel-exit]').click();

  await page.locator('#favourite-toggle').click();
  await page.waitForTimeout(40);
  const particles = await page.evaluate(() => {
    const stage = document.querySelector('.lightbox-stage').getBoundingClientRect();
    const container = document.querySelector('.heart-particles');
    const visible = Array.from(container.children).filter(particle => !particle.hidden);
    return {
      parent: container.parentElement.id,
      overflow: getComputedStyle(container).overflow,
      extendsBelowStage: visible.some(particle => particle.getBoundingClientRect().bottom > stage.bottom)
    };
  });
  expect(particles).toEqual({parent: 'image-lightbox', overflow: 'visible', extendsBelowStage: true});

  if (await page.locator('#favourite-toggle').getAttribute('aria-pressed') === 'false') {
    await page.locator('#favourite-toggle').click();
  }
  await page.locator('.lightbox-close').click();
  const favouriteMark = page.locator('.entry.image[data-favourite="true"] .favourite-mark').first();
  await expect(favouriteMark).toBeVisible();
  await expect(favouriteMark.locator('svg path')).toHaveCSS('fill', 'rgb(255, 79, 120)');
  await expect(favouriteMark).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)');
  await page.request.delete('/01-portrait.svg?mode=favourite');
});
