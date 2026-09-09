const { defineConfig, devices } = require('@playwright/test');

module.exports = defineConfig({
  testDir: './tests/browser',
  fullyParallel: false,
  workers: 1,
  timeout: 30_000,
  expect: { timeout: 5_000 },
  globalSetup: require.resolve('./tests/browser/global-setup'),
  globalTeardown: require.resolve('./tests/browser/global-teardown'),
  webServer: {
    command: 'cargo run --release -- -p 18180 -dir tests/browser/fixtures',
    url: 'http://127.0.0.1:18180/?view=gallery',
    reuseExistingServer: false,
    timeout: 120_000
  },
  use: {
    baseURL: 'http://127.0.0.1:18180',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure'
  },
  projects: [
    {
      name: 'desktop-chromium',
      testMatch: '**/*.desktop.spec.js',
      use: {...devices['Desktop Chrome'], viewport: {width: 1440, height: 900}}
    },
    {
      name: 'desktop-webkit',
      testMatch: '**/*.desktop.spec.js',
      use: {...devices['Desktop Safari'], viewport: {width: 1440, height: 900}}
    },
    {
      name: 'mobile-chromium',
      testMatch: '**/*.mobile.spec.js',
      use: {...devices['Pixel 7']}
    },
    {
      name: 'mobile-webkit',
      testMatch: '**/*.mobile.spec.js',
      use: {...devices['iPhone 13']}
    },
    {
      name: 'tablet-webkit',
      testMatch: '**/*.mobile.spec.js',
      use: {...devices['iPad Mini']}
    }
  ]
});
