(() => {
    // Only chapters have a markdown source; print.html and 404.html have no edit link either.
    const editLink = document.querySelector('.right-buttons a[rel="edit"]');
    if (!editLink) {
        return;
    }

    // The first chapter is served as index.html, but the markdown renderer keeps its source name.
    let path = window.location.pathname;
    if (path.endsWith('/') || path.endsWith('/index.html')) {
        path = path.replace(/(index\.html)?$/, 'introduction.md');
    } else {
        path = path.replace(/\.html$/, '.md');
    }

    const link = document.createElement('a');
    link.href = path;
    link.title = 'View this page as Markdown';
    link.setAttribute('aria-label', 'View this page as Markdown');
    link.innerHTML = '<span class="fa-svg"><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 640 512"><path d="M593.8 59.1H46.2C20.7 59.1 0 79.8 0 105.2v301.5c0 25.5 20.7 46.2 46.2 46.2h547.7c25.5 0 46.2-20.7 46.1-46.1V105.2c0-25.4-20.7-46.1-46.2-46.1zM338.5 360.6H277v-120l-61.5 76.9-61.5-76.9v120H92.3V151.4h61.5l61.5 76.9 61.5-76.9h61.5v209.2zm135.3 3.1L381.5 256H443V151.4h61.5V256H566z"/></svg></span>';
    editLink.before(link);
})();
