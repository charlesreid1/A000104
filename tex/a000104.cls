% Short-note class for OEIS attachments.
% Trimmed from the PE502 report class (pe502-pubs, PE502-4.cls): same fonts, page layout,
% section style, OEIS citations and reference list; no cover, page header or report number.

\NeedsTeXFormat{LaTeX2e}[2020-10-01]
\ProvidesClass{a000104}[2026-11-01 v1.0 OEIS note]

% A short note: the standard "article" class, plain numbered sections
\LoadClassWithOptions{article}

% Fonts for pdfLaTeX
\RequirePackage[T1]{fontenc}
\RequirePackage{lmodern}

% Page layout
\RequirePackage[margin=3cm]{geometry}

% No paragraph indent, space between paragraphs; no widows or orphans
\RequirePackage{parskip}
\clubpenalty=10000
\@clubpenalty=\clubpenalty
\widowpenalty=10000
\displaywidowpenalty=10000

% Section titles
\RequirePackage[medium]{titlesec}
\titleformat{\section}{\scshape\Large\vspace{-0.3em}}{\thesection}{0.3em}{}[\titlerule]

% Figures
\RequirePackage{graphicx}
\RequirePackage{xcolor}
\RequirePackage{tikz}
\definecolor{inkblue}{rgb}{0.173,0.243,0.314}
\colorlet{inkwall}{inkblue!20}

% Load hyperref last; pdfusetitle fills the PDF metadata from \title and \author
\RequirePackage[pdfusetitle]{hyperref}

% ------------------------------------------------------------------------------------------------
% OEIS sequences are cited by A-number, never by author and year:
%
%   \oeis{A000045}   prints "A000045" linked to https://oeis.org/A000045 and cites the .bib
%                    entry OEIS:A000045, so the reference list records the page.
%                    Works in text and in math mode.
%
% \printreferences prints the reference list: other works by author and year under
% "References", then "OEIS sequences" labeled by A-number and sorted by it. Each OEIS entry
% ends with the pages that cite it, linked back to those pages.
%
% .bib convention:
%   @online{OEIS:A000045, title = {Fibonacci numbers},
%           organization = {The On-Line Encyclopedia of Integer Sequences},
%           url = {https://oeis.org/A000045}, urldate = {...}}
%
% Requires biblatex, loaded by the document after this class.
% ------------------------------------------------------------------------------------------------
\RequirePackage{etoolbox}
\AtEndPreamble{%
  \@ifpackageloaded{biblatex}{%
    % Tag OEIS entries by key: OEIS:A<n> -> keyword oeisentry, A-number in usera.
    \DeclareSourcemap{%
      \maps[datatype=bibtex]{%
        \map[overwrite]{%
          \step[fieldsource=entrykey, match=\regexp{\AOEIS:(A[0-9]+)\Z}, final]
          \step[fieldset=keywords, fieldvalue={oeisentry}]
          \step[fieldset=usera, fieldvalue={$1}]
        }%
      }%
    }%
    % The A-number links to its OEIS page, not to the reference list, so a reader who
    % clicks it keeps their place in the PDF; the citation still records the page.
    \DeclareCiteCommand{\note@oeiscite}
      {}
      {\href{https://oeis.org/\thefield{usera}}{\printfield{usera}}}
      {\multicitedelim}
      {}%
    \DeclareRobustCommand*{\oeis}[1]{%
      \ifmmode\mbox{\note@oeiscite{OEIS:#1}}\else\note@oeiscite{OEIS:#1}\fi}%
    % Back-references: the OEIS entries print the pages that cite them.
    \ExecuteBibliographyOptions{backref=true, backrefstyle=none}%
    \DefineBibliographyStrings{english}{%
      backrefpage = {cited on page},
      backrefpages = {cited on pages},
    }%
    % Sorting: name, year, title, except that an OEIS entry sorts by its A-number.
    \DeclareSortingTemplate{notenyt}{%
      \sort{\field{presort}}%
      \sort[final]{\field[padside=left, padwidth=16, padchar=0]{usera}}%
      \sort[final]{\field{sortkey}}%
      \sort{\field{sortname}\field{author}\field{editor}\field{translator}\field{sorttitle}\field{title}}%
      \sort{\field{sortyear}\field{year}}%
      \sort{\field{sorttitle}\field{title}}%
      \sort{\field{volume}\literal{0}}%
    }%
    \ExecuteBibliographyOptions{sorting=notenyt}%
    % In the OEIS list the A-number does the work of author and year: no year, no dash for
    % a repeated author. Only the OEIS entries print back-references.
    \AtEveryBibitem{%
      \ifkeyword{oeisentry}{\clearfield{labelyear}\clearfield{extradate}\global\undef\bbx@lasthash}{\clearlist{pageref}}%
    }%
    % A list labeled by A-number.
    \defbibenvironment{oeislabeled}
      {\list{\printfield{usera}}
        {\settowidth{\labelwidth}{A000000}%
         \setlength{\leftmargin}{\labelwidth}%
         \addtolength{\leftmargin}{\labelsep}%
         \setlength{\itemsep}{\bibitemsep}%
         \setlength{\parsep}{\bibparsep}%
         \renewcommand*{\makelabel}[1]{##1\hss}}}
      {\endlist}
      {\item}%
    \defbibheading{notesubbibliography}[]{\subsection*{#1}}%
  }{}%
}
% Without biblatex, \oeis links straight to the OEIS.
\providecommand*{\oeis}[1]{\href{https://oeis.org/#1}{#1}}
\newcommand*{\printreferences}{%
  \printbibliography[heading=bibintoc, title={References}, notkeyword=oeisentry]%
  \printbibliography[heading=notesubbibliography, title={OEIS sequences},
    env=oeislabeled, keyword=oeisentry]%
}
