(function() {
  const modal = document.getElementById('create-modal');
  const titleInput = document.getElementById('create-title');
  const pathPreview = document.getElementById('create-path-preview');

  // Config from hidden inputs
  function getConfig() {
    return {
      repo: document.getElementById('create-github-repo').value,
      branch: document.getElementById('create-branch').value,
      subdir: document.getElementById('create-subdir').value,
      nextId: document.getElementById('create-next-id').value,
      format: document.getElementById('create-format').value,
      structure: document.getElementById('create-structure').value
    };
  }

  // Slugify a title to a URL-friendly format
  function slugify(text) {
    return text
      .toLowerCase()
      .normalize('NFD')
      .replace(/[\u0300-\u036f]/g, '') // Remove diacritics
      .replace(/[^a-z0-9]+/g, '-')     // Replace non-alphanumeric with dashes
      .replace(/^-+|-+$/g, '')          // Trim leading/trailing dashes
      .substring(0, 50);                // Limit length
  }

  // Build the file path based on config
  function buildPath(title) {
    const config = getConfig();
    const slug = slugify(title);
    if (!slug) return '...';

    const ext = config.format;
    const id = config.nextId;
    const prefix = config.subdir ? config.subdir + '/' : '';

    if (config.structure === 'flat') {
      // Flat: 0001-my-spec.md
      return prefix + id + '-' + slug + '.' + ext;
    } else {
      // Directory: 0001-my-spec/my-spec.md
      return prefix + id + '-' + slug + '/' + slug + '.' + ext;
    }
  }

  // Build GitHub new file URL
  function buildGitHubUrl(title) {
    const config = getConfig();
    const path = buildPath(title);
    if (path === '...') return null;

    return 'https://github.com/' + config.repo + '/new/' + config.branch + '?filename=' + encodeURIComponent(path);
  }

  // Update path preview as user types
  function updatePreview() {
    const title = titleInput.value.trim();
    pathPreview.textContent = buildPath(title);
  }

  // Open modal
  window.openCreateModal = function() {
    modal.hidden = false;
    titleInput.value = '';
    updatePreview();
    titleInput.focus();
  };

  // Close modal
  window.closeCreateModal = function() {
    modal.hidden = true;
  };

  // Handle form submit
  window.handleCreateSubmit = function(event) {
    event.preventDefault();
    const title = titleInput.value.trim();
    const url = buildGitHubUrl(title);
    if (url) {
      window.open(url, '_blank');
      closeCreateModal();
    }
  };

  // Update preview on input
  titleInput.addEventListener('input', updatePreview);

  // Close on Escape
  document.addEventListener('keydown', function(e) {
    if (e.key === 'Escape' && !modal.hidden) {
      closeCreateModal();
    }
  });
})();
