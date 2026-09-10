const { test, expect } = require('@playwright/test');
const { dispatchTouchPointer, openGalleryImage } = require('./helpers');

test('page jump buttons only appear while the page is scrolling', async ({page}) => {
  await page.goto('/?view=gallery');
  const scrollJumps = page.locator('#scroll-jumps');

  await expect(scrollJumps).toBeHidden();
  await page.evaluate(() => window.scrollTo(0, 200));
  await expect(scrollJumps).toBeVisible();
  await page.waitForTimeout(1000);
  await expect(scrollJumps).toBeHidden();
});

test('mobile directory favourites keep every shortcut reachable without overlapping folder stars', async ({page}) => {
  test.skip(page.viewportSize().width > 650, 'phone layout only');
  for (const suffix of ['', '-2', '-3', '-4']) {
    await page.request.delete(`/shortcut-folder${suffix}/?mode=directory-favourite`);
    await page.request.put(`/shortcut-folder${suffix}/?mode=directory-favourite`);
  }

  await page.goto('/');
  await expect(page.locator('.directory-favourites-list .directory-favourite')).toHaveCount(1);
  await expect(page.locator('.directory-favourites-more')).toBeVisible();
  await page.locator('.directory-favourites-more summary').click();
  await expect(page.locator('.directory-favourites-menu .directory-favourite')).toHaveCount(3);
  for (const item of await page.locator('.directory-favourites-menu .directory-favourite').all()) {
    await expect(item).toBeVisible();
  }

  const folderControls = await page.locator('.entry.folder').evaluateAll(entries => entries.map(entry => {
    const name = entry.querySelector('.entry-name').getBoundingClientRect();
    const star = entry.querySelector('.folder-favourite-toggle').getBoundingClientRect();
    return {
      overlap: name.left < star.right && name.right > star.left && name.top < star.bottom && name.bottom > star.top,
      detailVisible: getComputedStyle(entry.querySelector('.detail')).display !== 'none'
    };
  }));
  expect(folderControls.every(({overlap, detailVisible}) => !overlap && !detailVisible)).toBe(true);

  for (const suffix of ['', '-2', '-3', '-4']) {
    await page.request.delete(`/shortcut-folder${suffix}/?mode=directory-favourite`);
  }
});

test('screen-wide vertical swipe hands off without snapping back', async ({page}) => {
  await openGalleryImage(page);
  await expect(page.locator('#image-lightbox')).toHaveClass(/chrome-hidden/);
  await expect(page.locator('#image-lightbox figcaption')).toHaveCSS('opacity', '0');
  await expect(page.locator('.lightbox-position')).toBeVisible();
  await expect(page.locator('.lightbox-filmstrip')).toBeVisible();
  await expect(page.locator('.lightbox-close')).toBeVisible();
  await expect(page.locator('.lightbox-state')).toHaveCSS('opacity', '1');
  await expect(page.locator('.lightbox-favourite-state')).not.toHaveClass(/liked/);
  await expect(page.locator('.lightbox-deletion-state')).toBeHidden();
  const singleViewerSurface = await page.locator('#image-lightbox').evaluate(element => {
    const style = getComputedStyle(element);
    return {
      background: style.backgroundColor,
      backdropFilter: style.backdropFilter || style.webkitBackdropFilter,
      imageBackdropDisplay: getComputedStyle(element.querySelector('.carousel-backdrop')).display
    };
  });
  expect(singleViewerSurface.background).not.toBe('rgba(0, 0, 0, 0)');
  expect(singleViewerSurface.backdropFilter).toContain('blur(');
  expect(singleViewerSurface.imageBackdropDisplay).toBe('none');
  await page.locator('#image-lightbox').evaluate(element => { element.setPointerCapture = () => {}; });
  const box = await page.locator('.lightbox-stage').boundingBox();
  const x = box.x + box.width / 2;
  const startY = box.y + box.height * .76;
  const endY = box.y + box.height * .28;
  const before = await page.locator('#image-lightbox').getAttribute('data-file-path');

  await dispatchTouchPointer(page, '#image-lightbox', 'pointerdown', x, startY);
  await dispatchTouchPointer(page, '#image-lightbox', 'pointermove', x, endY);
  await dispatchTouchPointer(page, '#image-lightbox', 'pointerup', x, endY);
  const handoff = await page.evaluate(() => {
    const outgoing = document.querySelector('.preview-outgoing');
    const neighbour = document.querySelector('.gesture-neighbour');
    return {
      outgoing: Boolean(outgoing),
      neighbour: Boolean(neighbour),
      firstTransform: outgoing?.getAnimations()[0]?.effect.getKeyframes()[0]?.transform || ''
    };
  });
  expect(handoff.outgoing).toBe(true);
  expect(handoff.neighbour).toBe(true);
  expect(handoff.firstTransform).toContain('px');
  await expect(page.locator('#image-lightbox')).not.toHaveAttribute('data-file-path', before);
  await expect(page.locator('#image-lightbox')).toHaveClass(/chrome-hidden/);
  await expect(page.locator('.preview-outgoing')).toHaveCount(0);

  await dispatchTouchPointer(page, '#image-lightbox', 'pointerdown', x, startY, 2);
  await dispatchTouchPointer(page, '#image-lightbox', 'pointerup', x, startY, 2);
  await expect(page.locator('#image-lightbox')).not.toHaveClass(/chrome-hidden/);
  await expect(page.locator('#image-lightbox figcaption')).toHaveCSS('opacity', '1');
  await expect(page.locator('.lightbox-state')).toHaveCSS('opacity', '1');
  await dispatchTouchPointer(page, '#image-lightbox', 'pointerdown', x, startY, 3);
  await dispatchTouchPointer(page, '#image-lightbox', 'pointermove', x, endY, 3);
  await dispatchTouchPointer(page, '#image-lightbox', 'pointerup', x, endY, 3);
  await expect(page.locator('#image-lightbox')).not.toHaveClass(/chrome-hidden/);
  await page.waitForTimeout(2700);
  await expect(page.locator('#image-lightbox')).toHaveClass(/chrome-hidden/);
  await expect(page.locator('.lightbox-state')).toHaveCSS('opacity', '1');

  await page.keyboard.press('d');
  await expect(page.locator('#deletion-toggle')).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('.lightbox-deletion-state')).toBeVisible();
  await expect(page.locator('#delete-dialog')).toBeHidden();
  await page.waitForTimeout(450);
  await page.keyboard.press('d');
  await expect(page.locator('#deletion-toggle')).toHaveAttribute('aria-pressed', 'false');
  await expect(page.locator('#delete-dialog')).toBeHidden();
});

test('double-tapping the image toggles favourite state', async ({page}) => {
  await page.request.delete('/03-square.svg?mode=favourite');
  await openGalleryImage(page, '03-square.svg');
  await page.locator('#image-lightbox').evaluate(element => { element.setPointerCapture = () => {}; });
  const image = await page.locator('.lightbox-image').boundingBox();
  const x = image.x + image.width / 2;
  const y = image.y + image.height / 2;
  await expect(page.locator('#favourite-toggle')).toHaveAttribute('aria-pressed', 'false');
  await expect(page.locator('.lightbox-favourite-state')).toBeHidden();

  for (let pointerId = 1; pointerId <= 2; pointerId += 1) {
    await dispatchTouchPointer(page, '.lightbox-image', 'pointerdown', x, y, pointerId);
    await dispatchTouchPointer(page, '#image-lightbox', 'pointerup', x, y, pointerId);
  }

  const feedback = await page.locator('.heart-particles').evaluate(container => ({
    visible: Array.from(container.children).filter(particle => !particle.hidden).length,
    animations: container.getAnimations({subtree: true}).length
  }));
  expect(feedback.visible).toBeGreaterThan(0);
  expect(feedback.animations).toBeGreaterThan(0);
  await expect(page.locator('#favourite-toggle')).toHaveAttribute('aria-pressed', 'true');
  await expect(page.locator('.lightbox-favourite-state')).toBeVisible();
  await page.request.delete('/03-square.svg?mode=favourite');
});

test('mobile favourites organiser reports when there are no images to move', async ({page}) => {
  await Promise.all(['01-portrait.svg', '02-landscape.svg', '03-square.svg'].map(name =>
    page.request.delete(`/${name}?mode=favourite`)
  ));
  await page.goto('/?view=gallery');

  await page.locator('[data-move-images="favourites"]').click();
  await expect(page.locator('#directory-notice')).toHaveText('没有图片需要移动');
  await expect(page.locator('#directory-notice')).toBeVisible();
  await expect(page.locator('.move-dialog')).toBeHidden();
});

test('mobile comments open as a bottom drawer and keep typing isolated from shortcuts', async ({page}) => {
  await page.addInitScript(() => localStorage.setItem('webdir-review-identity', 'Alice'));
  await page.request.post('/02-landscape.svg?mode=gallery-comments', {data: {type: 'delete-all'}});
  for (let index = 0; index < 6; index += 1) {
    await page.request.post('/02-landscape.svg?mode=gallery-comments', {data: {
      type: 'add',
      comment: {
        id: `mobile-existing-${index}`,
        image: '02-landscape.svg',
        author: 'Alice',
        body: `移动端已有评论 ${index + 1}`,
        created_at: new Date(2026, 8, 9, 11, index).toISOString()
      }
    }});
  }
  await openGalleryImage(page, '02-landscape.svg');
  const pathBeforeTyping = await page.locator('#image-lightbox').getAttribute('data-file-path');

  await page.keyboard.press('c');
  await expect(page.locator('.gallery-comments')).toBeVisible();
  await expect(page.locator('.comment-toggle')).toHaveAttribute('aria-expanded', 'true');
  await page.locator('.gallery-comments').evaluate(element => Promise.all(
    element.getAnimations().map(animation => animation.finished)
  ));
  const drawer = await page.locator('.gallery-comments').boundingBox();
  const viewport = page.viewportSize();
  expect(drawer.x).toBeGreaterThanOrEqual(0);
  expect(drawer.x + drawer.width).toBeLessThanOrEqual(viewport.width);
  expect(drawer.y + drawer.height).toBeLessThanOrEqual(viewport.height);
  expect(viewport.height - drawer.y - drawer.height).toBeLessThanOrEqual(16);

  const editor = page.locator('.gallery-comment-composer textarea');
  const list = page.locator('.gallery-comment-list');
  await expect(list.locator('article')).toHaveCount(6);
  await expect.poll(() => list.evaluate(element =>
    element.scrollHeight - element.scrollTop - element.clientHeight
  )).toBeLessThanOrEqual(1);
  await expect(editor).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(editor).not.toBeFocused();
  await expect(page.locator('.gallery-comments')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.locator('.gallery-comments')).toBeHidden();
  await page.keyboard.press('c');
  await expect(editor).toBeFocused();
  await page.keyboard.type('jkcmp');
  await editor.press('c');
  await expect(page.locator('.gallery-comments')).toBeVisible();
  await expect(editor).toHaveValue('jkcmpc');
  await expect(page.locator('#image-lightbox')).toHaveAttribute('data-file-path', pathBeforeTyping);
  await editor.press('Enter');
  await expect(list.locator('article').last().locator('p')).toHaveText('jkcmpc');

  await page.locator('[data-comment-delete-all]').click();
  await page.locator('[data-comment-delete-all]').click();
  await expect(page.locator('.gallery-comment-list article')).toHaveCount(0);
});

test('mobile slideshow replaces viewer chrome with the image backdrop', async ({page}) => {
  await openGalleryImage(page);
  await page.locator('#image-lightbox').evaluate(element => {
    Object.defineProperty(element, 'requestFullscreen', {value: () => Promise.resolve(), configurable: true});
  });

  await page.keyboard.press('p');
  await expect(page.locator('#image-lightbox')).toHaveClass(/carousel-mode/);
  await expect(page.locator('.carousel-backdrop')).toBeVisible();
  await expect(page.locator('.lightbox-close')).toBeHidden();
  await expect(page.locator('.lightbox-position')).toBeHidden();
  await expect(page.locator('.lightbox-filmstrip')).toBeHidden();
  await expect(page.locator('#image-lightbox figcaption')).toBeHidden();
  const slideshowSurface = await page.locator('#image-lightbox').evaluate(element => {
    const surface = getComputedStyle(element);
    const backdrop = getComputedStyle(element.querySelector('.carousel-backdrop'));
    return {
      background: surface.backgroundColor,
      backdropFilter: surface.backdropFilter || surface.webkitBackdropFilter,
      backdropImage: backdrop.backgroundImage,
      backdropFilterImage: backdrop.filter
    };
  });
  expect(slideshowSurface.background).toBe('rgb(0, 0, 0)');
  expect(slideshowSurface.backdropFilter).toBe('none');
  expect(slideshowSurface.backdropImage).not.toBe('none');
  expect(slideshowSurface.backdropFilterImage).toContain('blur(12px)');

  await page.keyboard.press('p');
  await expect(page.locator('#image-lightbox')).not.toHaveClass(/carousel-mode/);
  await expect(page.locator('.carousel-backdrop')).toBeHidden();
});
