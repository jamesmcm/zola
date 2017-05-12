use std::collections::{HashMap};
use std::iter::FromIterator;
use std::fs::{remove_dir_all, copy, create_dir_all};
use std::path::{Path, PathBuf};

use glob::glob;
use tera::{Tera, Context};
use slug::slugify;
use walkdir::WalkDir;

use errors::{Result, ResultExt};
use config::{Config, get_config};
use page::{Page, populate_previous_and_next_pages, sort_pages};
use pagination::Paginator;
use utils::{create_file, create_directory};
use section::{Section};
use front_matter::{SortBy};
use filters;
use global_fns;


lazy_static! {
    pub static ref GUTENBERG_TERA: Tera = {
        let mut tera = Tera::default();
        tera.add_raw_templates(vec![
            ("rss.xml", include_str!("templates/rss.xml")),
            ("sitemap.xml", include_str!("templates/sitemap.xml")),
            ("robots.txt", include_str!("templates/robots.txt")),
            ("anchor-link.html", include_str!("templates/anchor-link.html")),

            ("shortcodes/youtube.html", include_str!("templates/shortcodes/youtube.html")),
            ("shortcodes/vimeo.html", include_str!("templates/shortcodes/vimeo.html")),
            ("shortcodes/gist.html", include_str!("templates/shortcodes/gist.html")),

            ("internal/alias.html", include_str!("templates/internal/alias.html")),
        ]).unwrap();
        tera
    };
}

/// Renders the `internal/alias.html` template that will redirect
/// via refresh to the url given
fn render_alias(url: &str, tera: &Tera) -> Result<String> {
    let mut context = Context::new();
    context.add("url", &url);

    tera.render("internal/alias.html", &context)
        .chain_err(|| format!("Failed to render alias for '{}'", url))
}


#[derive(Debug, PartialEq)]
enum RenderList {
    Tags,
    Categories,
}

/// A tag or category
#[derive(Debug, Serialize, PartialEq)]
struct ListItem {
    name: String,
    slug: String,
    count: usize,
}

impl ListItem {
    pub fn new(name: &str, count: usize) -> ListItem {
        ListItem {
            name: name.to_string(),
            slug: slugify(name),
            count: count,
        }
    }
}

#[derive(Debug)]
pub struct Site {
    pub base_path: PathBuf,
    pub config: Config,
    pub pages: HashMap<PathBuf, Page>,
    pub sections: HashMap<PathBuf, Section>,
    pub tera: Tera,
    live_reload: bool,
    output_path: PathBuf,
    static_path: PathBuf,
    pub tags: HashMap<String, Vec<PathBuf>>,
    pub categories: HashMap<String, Vec<PathBuf>>,
    pub permalinks: HashMap<String, String>,
}

impl Site {
    /// Parse a site at the given path. Defaults to the current dir
    /// Passing in a path is only used in tests
    pub fn new<P: AsRef<Path>>(path: P, config_file: &str) -> Result<Site> {
        let path = path.as_ref();

        let tpl_glob = format!("{}/{}", path.to_string_lossy().replace("\\", "/"), "templates/**/*.*ml");
        let mut tera = Tera::new(&tpl_glob).chain_err(|| "Error parsing templates")?;
        tera.extend(&GUTENBERG_TERA)?;
        tera.register_filter("markdown", filters::markdown);
        tera.register_filter("base64_encode", filters::base64_encode);
        tera.register_filter("base64_decode", filters::base64_decode);

        let site = Site {
            base_path: path.to_path_buf(),
            config: get_config(path, config_file),
            pages: HashMap::new(),
            sections: HashMap::new(),
            tera: tera,
            live_reload: false,
            output_path: path.join("public"),
            static_path: path.join("static"),
            tags: HashMap::new(),
            categories: HashMap::new(),
            permalinks: HashMap::new(),
        };

        Ok(site)
    }

    /// What the function name says
    pub fn enable_live_reload(&mut self) {
        self.live_reload = true;
    }

    /// Gets the path of all ignored pages in the site
    pub fn get_ignored_pages(&self) -> Vec<PathBuf> {
        self.sections
            .values()
            .flat_map(|s| s.ignored_pages.iter().map(|p| p.file_path.clone()))
            .collect()
    }

    /// Get all the orphan (== without section) pages in the site
    pub fn get_all_orphan_pages(&self) -> Vec<&Page> {
        let mut pages_in_sections = vec![];
        let mut orphans = vec![];

        for s in self.sections.values() {
            pages_in_sections.extend(s.all_pages_path());
        }

        for page in self.pages.values() {
            if !pages_in_sections.contains(&page.file_path) {
                orphans.push(page);
            }
        }

        orphans
    }

    /// Used by tests to change the output path to a tmp dir
    #[doc(hidden)]
    pub fn set_output_path<P: AsRef<Path>>(&mut self, path: P) {
        self.output_path = path.as_ref().to_path_buf();
    }

    /// Reads all .md files in the `content` directory and create pages/sections
    /// out of them
    pub fn load(&mut self) -> Result<()> {
        let base_path = self.base_path.to_string_lossy().replace("\\", "/");
        let content_glob = format!("{}/{}", base_path, "content/**/*.md");

        // TODO: make that parallel, that's the main bottleneck
        // `add_section` and `add_page` can't be used in the parallel version afaik
        for entry in glob(&content_glob).unwrap().filter_map(|e| e.ok()) {
            let path = entry.as_path();
            if path.file_name().unwrap() == "_index.md" {
                self.add_section(path)?;
            } else {
                self.add_page(path)?;
            }
        }
        // Insert a default index section so we don't need to create a _index.md to render
        // the index page
        let index_path = self.base_path.join("content").join("_index.md");
        if !self.sections.contains_key(&index_path) {
            let mut index_section = Section::default();
            index_section.permalink = self.config.make_permalink("");
            self.sections.insert(index_path, index_section);
        }

        // A map of all .md files (section and pages) and their permalink
        // We need that if there are relative links in the content that need to be resolved
        let mut permalinks = HashMap::new();

        for page in self.pages.values() {
            permalinks.insert(page.relative_path.clone(), page.permalink.clone());
        }

        for section in self.sections.values() {
            permalinks.insert(section.relative_path.clone(), section.permalink.clone());
        }

        for page in self.pages.values_mut() {
            page.render_markdown(&permalinks, &self.tera, &self.config)?;
        }

        for section in self.sections.values_mut() {
            section.render_markdown(&permalinks, &self.tera, &self.config)?;
        }

        self.permalinks = permalinks;
        self.populate_sections();
        self.populate_tags_and_categories();

        self.tera.register_global_function("get_page", global_fns::make_get_page(&self.pages));

        Ok(())
    }

    /// Simple wrapper fn to avoid repeating that code in several places
    fn add_page(&mut self, path: &Path) -> Result<()> {
        let page = Page::from_file(&path, &self.config)?;
        self.pages.insert(page.file_path.clone(), page);
        Ok(())
    }

    /// Simple wrapper fn to avoid repeating that code in several places
    fn add_section(&mut self, path: &Path) -> Result<()> {
        let section = Section::from_file(path, &self.config)?;
        self.sections.insert(section.file_path.clone(), section);
        Ok(())
    }

    /// Called in serve, add the section and render it
    fn add_section_and_render(&mut self, path: &Path) -> Result<()> {
        self.add_section(path)?;
        let mut section = self.sections.get_mut(path).unwrap();
        self.permalinks.insert(section.relative_path.clone(), section.permalink.clone());
        section.render_markdown(&self.permalinks, &self.tera, &self.config)?;
        Ok(())
    }

    /// Called in serve, add a page again updating permalinks and its content
    /// The bool in the result is whether the front matter has been updated or not
    /// TODO: the above is very confusing, change that
    fn add_page_and_render(&mut self, path: &Path) -> Result<(bool, Page)> {
        let existing_page = self.pages.get(path).cloned();
        self.add_page(path)?;
        let mut page = self.pages.get_mut(path).unwrap();
        self.permalinks.insert(page.relative_path.clone(), page.permalink.clone());
        page.render_markdown(&self.permalinks, &self.tera, &self.config)?;

        if let Some(prev_page) = existing_page {
            return Ok((prev_page.meta != page.meta, page.clone()));
        }
        Ok((true, page.clone()))
    }

    /// Find out the direct subsections of each subsection if there are some
    /// as well as the pages for each section
    pub fn populate_sections(&mut self) {
        for page in self.pages.values() {
            if self.sections.contains_key(&page.parent_path.join("_index.md")) {
                self.sections.get_mut(&page.parent_path.join("_index.md")).unwrap().pages.push(page.clone());
            }
        }

        let mut grandparent_paths = HashMap::new();
        for section in self.sections.values() {
            if let Some(grand_parent) = section.parent_path.parent() {
                grandparent_paths.entry(grand_parent.to_path_buf()).or_insert_with(|| vec![]).push(section.clone());
            }
        }

        for section in self.sections.values_mut() {
            // TODO: avoid this clone
            let (mut sorted_pages, cannot_be_sorted_pages) = sort_pages(section.pages.clone(), section.meta.sort_by());
            sorted_pages = populate_previous_and_next_pages(&sorted_pages);
            section.pages = sorted_pages;
            section.ignored_pages = cannot_be_sorted_pages;

            match grandparent_paths.get(&section.parent_path) {
                Some(paths) => section.subsections.extend(paths.clone()),
                None => continue,
            };
        }
    }

    /// Separated from `parse` for easier testing
    pub fn populate_tags_and_categories(&mut self) {
        for page in self.pages.values() {
            if let Some(ref category) = page.meta.category {
                self.categories
                    .entry(category.to_string())
                    .or_insert_with(|| vec![])
                    .push(page.file_path.clone());
            }

            if let Some(ref tags) = page.meta.tags {
                for tag in tags {
                    self.tags
                        .entry(tag.to_string())
                        .or_insert_with(|| vec![])
                        .push(page.file_path.clone());
                }
            }
        }
    }

    /// Inject live reload script tag if in live reload mode
    fn inject_livereload(&self, html: String) -> String {
        if self.live_reload {
            return html.replace(
                "</body>",
                r#"<script src="/livereload.js?port=1112&mindelay=10"></script></body>"#
            );
        }

        html
    }

    pub fn ensure_public_directory_exists(&self) -> Result<()> {
        let public = self.output_path.clone();
        if !public.exists() {
            create_directory(&public)?;
        }
        Ok(())
    }

    /// Copy static file to public directory.
    pub fn copy_static_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let relative_path = path.as_ref().strip_prefix(&self.static_path).unwrap();
        let target_path = self.output_path.join(relative_path);
        if let Some(parent_directory) = target_path.parent() {
            create_dir_all(parent_directory)?;
        }
        copy(path.as_ref(), &target_path)?;
        Ok(())
    }

    /// Copy the content of the `static` folder into the `public` folder
    pub fn copy_static_directory(&self) -> Result<()> {
        for entry in WalkDir::new(&self.static_path).into_iter().filter_map(|e| e.ok()) {
            let relative_path = entry.path().strip_prefix(&self.static_path).unwrap();
            let target_path = self.output_path.join(relative_path);

            if entry.path().is_dir() {
                if !target_path.exists() {
                    create_directory(&target_path)?;
                }
            } else {
                let entry_fullpath = self.base_path.join(entry.path());
                self.copy_static_file(entry_fullpath)?;
            }
        }
        Ok(())
    }

    /// Deletes the `public` directory if it exists
    pub fn clean(&self) -> Result<()> {
        if self.output_path.exists() {
            // Delete current `public` directory so we can start fresh
            remove_dir_all(&self.output_path).chain_err(|| "Couldn't delete `public` directory")?;
        }

        Ok(())
    }

    pub fn rebuild_after_content_change(&mut self, path: &Path) -> Result<()> {
        let is_section = path.ends_with("_index.md");

        if path.exists() {
            // file exists, either a new one or updating content
            if is_section {
                self.add_section_and_render(path)?;
                self.render_sections()?;
            } else {
                // probably just an update so just re-parse that page
                let (frontmatter_changed, page) = self.add_page_and_render(path)?;
                // TODO: can probably be smarter and check what changed
                if frontmatter_changed {
                    self.populate_sections();
                    self.populate_tags_and_categories();
                    self.build()?;
                } else {
                    self.render_page(&page)?;
                }
            }
        } else {
            // File doesn't exist -> a deletion so we remove it from everything
            let relative_path = if is_section {
                self.sections[path].relative_path.clone()
            } else {
                self.pages[path].relative_path.clone()
            };
            self.permalinks.remove(&relative_path);

            if is_section {
                self.sections.remove(path);
            } else {
                self.pages.remove(path);
            }
            // TODO: probably no need to do that, we should be able to only re-render a page or a section.
            self.populate_sections();
            self.populate_tags_and_categories();
            self.build()?;
        }

        Ok(())
    }

    pub fn rebuild_after_template_change(&mut self, path: &Path) -> Result<()> {
        self.tera.full_reload()?;
        match path.file_name().unwrap().to_str().unwrap() {
            "sitemap.xml" => self.render_sitemap(),
            "rss.xml" => self.render_rss_feed(),
            _ => self.build() // TODO: change that
        }
    }

    /// Renders a single content page
    pub fn render_page(&self, page: &Page) -> Result<()> {
        self.ensure_public_directory_exists()?;

        // Copy the nesting of the content directory if we have sections for that page
        let mut current_path = self.output_path.to_path_buf();

        for component in page.path.split('/') {
            current_path.push(component);

            if !current_path.exists() {
                create_directory(&current_path)?;
            }
        }

        // Make sure the folder exists
        create_directory(&current_path)?;

        // Finally, create a index.html file there with the page rendered
        let output = page.render_html(&self.tera, &self.config)?;
        create_file(current_path.join("index.html"), &self.inject_livereload(output))?;

        // Copy any asset we found previously into the same directory as the index.html
        for asset in &page.assets {
            let asset_path = asset.as_path();
            copy(&asset_path, &current_path.join(asset_path.file_name().unwrap()))?;
        }

        Ok(())
    }

    /// Builds the site to the `public` directory after deleting it
    pub fn build(&self) -> Result<()> {
        self.clean()?;
        self.render_sections()?;
        self.render_orphan_pages()?;
        self.render_sitemap()?;
        if self.config.generate_rss.unwrap() {
            self.render_rss_feed()?;
        }
        self.render_robots()?;
        if self.config.generate_categories_pages.unwrap() {
            self.render_categories_and_tags(RenderList::Categories)?;
        }
        if self.config.generate_tags_pages.unwrap() {
            self.render_categories_and_tags(RenderList::Tags)?;
        }

        self.copy_static_directory()
    }

    /// Renders robots.txt
    fn render_robots(&self) -> Result<()> {
        self.ensure_public_directory_exists()?;
        create_file(
            self.output_path.join("robots.txt"),
            &self.tera.render("robots.txt", &Context::new())?
        )
    }

    /// Render the /{categories, list} pages and each individual category/tag page
    /// They are the same thing fundamentally, a list of pages with something in common
    fn render_categories_and_tags(&self, kind: RenderList) -> Result<()> {
        let items = match kind {
            RenderList::Categories => &self.categories,
            RenderList::Tags => &self.tags,
        };

        if items.is_empty() {
            return Ok(());
        }

        let (list_tpl_name, single_tpl_name, name, var_name) = if kind == RenderList::Categories {
            ("categories.html", "category.html", "categories", "category")
        } else {
            ("tags.html", "tag.html", "tags", "tag")
        };
        self.ensure_public_directory_exists()?;

        // Create the categories/tags directory first
        let public = self.output_path.clone();
        let mut output_path = public.to_path_buf();
        output_path.push(name);
        create_directory(&output_path)?;

        // Then render the index page for that kind.
        // We sort by number of page in that category/tag
        let mut sorted_items = vec![];
        for (item, count) in Vec::from_iter(items).into_iter().map(|(a, b)| (a, b.len())) {
            sorted_items.push(ListItem::new(item, count));
        }
        sorted_items.sort_by(|a, b| b.count.cmp(&a.count));
        let mut context = Context::new();
        context.add(name, &sorted_items);
        context.add("config", &self.config);
        context.add("current_url", &self.config.make_permalink(name));
        context.add("current_path", &format!("/{}", name));
        // And render it immediately
        let list_output = self.tera.render(list_tpl_name, &context)?;
        create_file(output_path.join("index.html"), &self.inject_livereload(list_output))?;

        // Now, each individual item
        for (item_name, pages_paths) in items.iter() {
            let pages: Vec<&Page> = self.pages
                .iter()
                .filter(|&(path, _)| pages_paths.contains(path))
                .map(|(_, page)| page)
                .collect();
            // TODO: how to sort categories and tag content?
            // Have a setting in config.toml or a _category.md and _tag.md
            // The latter is more in line with the rest of Gutenberg but order ordering
            // doesn't really work across sections.

            let mut context = Context::new();
            let slug = slugify(&item_name);
            context.add(var_name, &item_name);
            context.add(&format!("{}_slug", var_name), &slug);
            context.add("pages", &pages);
            context.add("config", &self.config);
            context.add("current_url", &self.config.make_permalink(&format!("{}/{}", name, slug)));
            context.add("current_path", &format!("/{}/{}", name, slug));
            let single_output = self.tera.render(single_tpl_name, &context)?;

            create_directory(&output_path.join(&slug))?;
            create_file(
                output_path.join(&slug).join("index.html"),
                &self.inject_livereload(single_output)
            )?;
        }

        Ok(())
    }

    fn render_sitemap(&self) -> Result<()> {
        self.ensure_public_directory_exists()?;
        let mut context = Context::new();
        context.add("pages", &self.pages.values().collect::<Vec<&Page>>());
        context.add("sections", &self.sections.values().collect::<Vec<&Section>>());

        let mut categories = vec![];
        if self.config.generate_categories_pages.unwrap() && !self.categories.is_empty() {
            categories.push(self.config.make_permalink("categories"));
            for category in self.categories.keys() {
                categories.push(
                    self.config.make_permalink(&format!("categories/{}", slugify(category)))
                );
            }
        }
        context.add("categories", &categories);

        let mut tags = vec![];
        if self.config.generate_tags_pages.unwrap() && !self.tags.is_empty() {
            tags.push(self.config.make_permalink("tags"));
            for tag in self.tags.keys() {
                tags.push(
                    self.config.make_permalink(&format!("tags/{}", slugify(tag)))
                );
            }
        }
        context.add("tags", &tags);

        let sitemap = self.tera.render("sitemap.xml", &context)?;

        create_file(self.output_path.join("sitemap.xml"), &sitemap)?;

        Ok(())
    }

    fn render_rss_feed(&self) -> Result<()> {
        self.ensure_public_directory_exists()?;

        let mut context = Context::new();
        let pages = self.pages.values()
            .filter(|p| p.meta.date.is_some())
            .take(15) // limit to the last 15 elements
            .cloned()
            .collect::<Vec<Page>>();

        // Don't generate a RSS feed if none of the pages has a date
        if pages.is_empty() {
            return Ok(());
        }
        context.add("last_build_date", &pages[0].meta.date);
        let (sorted_pages, _) = sort_pages(pages, SortBy::Date);
        context.add("pages", &sorted_pages);
        context.add("config", &self.config);

        let rss_feed_url = if self.config.base_url.ends_with('/') {
            format!("{}{}", self.config.base_url, "rss.xml")
        } else {
            format!("{}/{}", self.config.base_url, "rss.xml")
        };
        context.add("feed_url", &rss_feed_url);

        let sitemap = self.tera.render("rss.xml", &context)?;

        create_file(self.output_path.join("rss.xml"), &sitemap)?;

        Ok(())
    }

    fn render_sections(&self) -> Result<()> {
        self.ensure_public_directory_exists()?;
        let public = self.output_path.clone();
        let sections: HashMap<String, Section> = self.sections
            .values()
            .map(|s| (s.components.join("/"), s.clone()))
            .collect();

        for section in self.sections.values() {
            let mut output_path = public.to_path_buf();
            for component in &section.components {
                output_path.push(component);

                if !output_path.exists() {
                    create_directory(&output_path)?;
                }
            }

            for page in &section.pages {
                self.render_page(page)?;
            }

            if !section.meta.should_render() {
                continue;
            }

            if section.meta.is_paginated() {
                self.render_paginated(&output_path, section)?;
            } else {
                let output = section.render_html(
                    &sections,
                    &self.tera,
                    &self.config,
                )?;
                create_file(output_path.join("index.html"), &self.inject_livereload(output))?;
            }
        }

        Ok(())
    }

    /// Renders all pages that do not belong to any sections
    fn render_orphan_pages(&self) -> Result<()> {
        self.ensure_public_directory_exists()?;

        for page in self.get_all_orphan_pages() {
            self.render_page(page)?;
        }

        Ok(())
    }

    /// Renders a list of pages when the section/index is wanting pagination.
    fn render_paginated(&self, output_path: &Path, section: &Section) -> Result<()> {
        self.ensure_public_directory_exists()?;

        let paginate_path = match section.meta.paginate_path {
            Some(ref s) => s.clone(),
            None => unreachable!()
        };

        let paginator = Paginator::new(&section.pages, section);
        for (i, pager) in paginator.pagers.iter().enumerate() {
            let folder_path = output_path.join(&paginate_path);
            let page_path = folder_path.join(&format!("{}", i + 1));
            create_directory(&folder_path)?;
            create_directory(&page_path)?;
            let output = paginator.render_pager(pager, self)?;
            if i > 0 {
                create_file(page_path.join("index.html"), &self.inject_livereload(output))?;
            } else {
                create_file(output_path.join("index.html"), &self.inject_livereload(output))?;
                create_file(page_path.join("index.html"), &render_alias(&section.permalink, &self.tera)?)?;
            }
        }

        Ok(())
    }
}
