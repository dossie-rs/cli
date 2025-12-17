  const searchInput = document.querySelector('#spec-search');
  const specItems = Array.from(document.querySelectorAll('.spec-list li'));
  const emptyMessage = document.querySelector('.filter-empty');

  const filterByTitle = (query) => {
    const normalized = query.trim().toLowerCase();
    const normalizedId = normalized.startsWith('#') ? normalized.slice(1) : normalized;
    let visible = 0;

    const matches = (value) =>
      value.includes(normalized) ||
      (normalizedId !== normalized && value.includes(normalizedId));

    specItems.forEach((item) => {
      const title = item.getAttribute('data-title') ?? '';
      const id = item.getAttribute('data-id') ?? '';
      const authors = item.getAttribute('data-authors') ?? '';
      const match =
        normalized === '' ||
        matches(title) ||
        matches(id) ||
        matches(authors);
      item.style.display = match ? '' : 'none';
      if (match) visible += 1;
    });

    if (emptyMessage) {
      const hasNoResults = visible === 0 && normalized !== '';
      emptyMessage.hidden = !hasNoResults;
      if (hasNoResults) {
        emptyMessage.textContent = `No specs match \"${query}\".`;
      }
    }
  };

  const focusSearch = () => {
    if (searchInput) {
      searchInput.focus();
      searchInput.select();
    }
  };

  if (searchInput) {
    filterByTitle(searchInput.value || '');
    searchInput.addEventListener('input', (event) => {
      filterByTitle(event.target.value);
    });
  }

  window.addEventListener('keydown', (event) => {
    if (event.key !== '/' || event.metaKey || event.ctrlKey || event.altKey) return;
    const active = document.activeElement;
    const isTyping =
      active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA' || active.isContentEditable);
    if (!isTyping) {
      event.preventDefault();
      focusSearch();
    }
  });
