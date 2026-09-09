const fs = require('node:fs');
const path = require('node:path');

module.exports = async () => {
  const fixtures = path.join(__dirname, 'fixtures');
  for (const name of ['.gallery-comments.json', 'review.md.review.json']) {
    fs.rmSync(path.join(fixtures, name), {force: true});
  }
};
