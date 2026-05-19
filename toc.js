// Populate the sidebar
//
// This is a script, and not included directly in the page, to control the total size of the book.
// The TOC contains an entry for each page, so if each page includes a copy of the TOC,
// the total size of the page becomes O(n**2).
class MDBookSidebarScrollbox extends HTMLElement {
    constructor() {
        super();
    }
    connectedCallback() {
        this.innerHTML = '<ol class="chapter"><li class="chapter-item expanded "><a href="index.html"><strong aria-hidden="true">1.</strong> Introduction</a></li><li class="chapter-item expanded "><a href="usage.html"><strong aria-hidden="true">2.</strong> Getting Started</a></li><li class="chapter-item expanded "><a href="why-jacquard.html"><strong aria-hidden="true">3.</strong> Why Jacquard</a></li><li class="chapter-item expanded "><a href="simulation-architecture.html"><strong aria-hidden="true">4.</strong> Simulation Architecture</a></li><li class="chapter-item expanded "><a href="timing-simulation.html"><strong aria-hidden="true">5.</strong> Timing Simulation</a></li><li class="chapter-item expanded "><a href="timing-violations.html"><strong aria-hidden="true">6.</strong> Timing Violations</a></li><li class="chapter-item expanded "><a href="timing-model-extensions.html"><strong aria-hidden="true">7.</strong> Timing Model Extensions</a></li><li class="chapter-item expanded "><a href="adding-a-pdk.html"><strong aria-hidden="true">8.</strong> Adding a New PDK</a></li><li class="chapter-item expanded "><a href="troubleshooting-vcd.html"><strong aria-hidden="true">9.</strong> Troubleshooting VCD</a></li><li class="chapter-item expanded "><a href="handoff-discipline.html"><strong aria-hidden="true">10.</strong> Handoff Discipline</a></li><li class="chapter-item expanded affix "><li class="part-title">Decisions &amp; plans</li><li class="chapter-item expanded "><a href="adr/index.html"><strong aria-hidden="true">11.</strong> Architecture Decision Records</a></li><li><ol class="section"><li class="chapter-item expanded "><a href="adr/0001-opensta-as-oracle.html"><strong aria-hidden="true">11.1.</strong> 0001 — OpenSTA as the timing correctness oracle</a></li><li class="chapter-item expanded "><a href="adr/0002-timing-ir.html"><strong aria-hidden="true">11.2.</strong> 0002 — Timing intermediate representation</a></li><li class="chapter-item expanded "><a href="adr/0003-opentimer-primary-sta.html"><strong aria-hidden="true">11.3.</strong> 0003 — OpenTimer as in-process reference STA (Superseded)</a></li><li class="chapter-item expanded "><a href="adr/0004-private-pdk-testing.html"><strong aria-hidden="true">11.4.</strong> 0004 — Private PDK testing track</a></li><li class="chapter-item expanded "><a href="adr/0005-opensta-vendoring-and-corpus.html"><strong aria-hidden="true">11.5.</strong> 0005 — OpenSTA vendoring and test-corpus strategy</a></li><li class="chapter-item expanded "><a href="adr/0006-sdf-preprocessing-model.html"><strong aria-hidden="true">11.6.</strong> 0006 — SDF preprocessing model</a></li><li class="chapter-item expanded "><a href="adr/0007-timing-model-fidelity-roadmap.html"><strong aria-hidden="true">11.7.</strong> 0007 — Timing model fidelity roadmap (Proposed)</a></li><li class="chapter-item expanded "><a href="adr/0008-structured-timing-output.html"><strong aria-hidden="true">11.8.</strong> 0008 — Structured timing output as first-class deliverable</a></li></ol></li><li class="chapter-item expanded "><a href="plans/index.html"><strong aria-hidden="true">12.</strong> Implementation Plans</a></li><li><ol class="section"><li class="chapter-item expanded "><a href="plans/post-phase-0-roadmap.html"><strong aria-hidden="true">12.1.</strong> Post-Phase-0 Roadmap</a></li><li class="chapter-item expanded "><a href="plans/gf180mcu-enablement.html"><strong aria-hidden="true">12.2.</strong> GF180MCU PDK enablement</a></li><li class="chapter-item expanded "><a href="plans/phase-0-ir-and-oracle.html"><strong aria-hidden="true">12.3.</strong> Phase 0: Timing IR and OpenSTA oracle</a></li><li class="chapter-item expanded "><a href="plans/ws2-opensta-to-ir.html"><strong aria-hidden="true">12.4.</strong> WS2: opensta-to-ir</a></li><li class="chapter-item expanded "><a href="plans/ws3-delete-sdf-parser.html"><strong aria-hidden="true">12.5.</strong> WS3: delete SDF parser</a></li><li class="chapter-item expanded "><a href="plans/ws3-cosim-sdf-followup.html"><strong aria-hidden="true">12.6.</strong> WS3 follow-up: cosim --sdf</a></li><li class="chapter-item expanded "><a href="plans/multi-clock-and-stimulus-architecture.html"><strong aria-hidden="true">12.7.</strong> Multi-clock and stimulus architecture</a></li></ol></li><li class="chapter-item expanded "><a href="spikes/opentimer-sky130.html"><strong aria-hidden="true">13.</strong> Spike — OpenTimer on SKY130</a></li></ol>';
        // Set the current, active page, and reveal it if it's hidden
        let current_page = document.location.href.toString().split("#")[0];
        if (current_page.endsWith("/")) {
            current_page += "index.html";
        }
        var links = Array.prototype.slice.call(this.querySelectorAll("a"));
        var l = links.length;
        for (var i = 0; i < l; ++i) {
            var link = links[i];
            var href = link.getAttribute("href");
            if (href && !href.startsWith("#") && !/^(?:[a-z+]+:)?\/\//.test(href)) {
                link.href = path_to_root + href;
            }
            // The "index" page is supposed to alias the first chapter in the book.
            if (link.href === current_page || (i === 0 && path_to_root === "" && current_page.endsWith("/index.html"))) {
                link.classList.add("active");
                var parent = link.parentElement;
                if (parent && parent.classList.contains("chapter-item")) {
                    parent.classList.add("expanded");
                }
                while (parent) {
                    if (parent.tagName === "LI" && parent.previousElementSibling) {
                        if (parent.previousElementSibling.classList.contains("chapter-item")) {
                            parent.previousElementSibling.classList.add("expanded");
                        }
                    }
                    parent = parent.parentElement;
                }
            }
        }
        // Track and set sidebar scroll position
        this.addEventListener('click', function(e) {
            if (e.target.tagName === 'A') {
                sessionStorage.setItem('sidebar-scroll', this.scrollTop);
            }
        }, { passive: true });
        var sidebarScrollTop = sessionStorage.getItem('sidebar-scroll');
        sessionStorage.removeItem('sidebar-scroll');
        if (sidebarScrollTop) {
            // preserve sidebar scroll position when navigating via links within sidebar
            this.scrollTop = sidebarScrollTop;
        } else {
            // scroll sidebar to current active section when navigating via "next/previous chapter" buttons
            var activeSection = document.querySelector('#sidebar .active');
            if (activeSection) {
                activeSection.scrollIntoView({ block: 'center' });
            }
        }
        // Toggle buttons
        var sidebarAnchorToggles = document.querySelectorAll('#sidebar a.toggle');
        function toggleSection(ev) {
            ev.currentTarget.parentElement.classList.toggle('expanded');
        }
        Array.from(sidebarAnchorToggles).forEach(function (el) {
            el.addEventListener('click', toggleSection);
        });
    }
}
window.customElements.define("mdbook-sidebar-scrollbox", MDBookSidebarScrollbox);
