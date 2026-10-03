//! Postman's dynamic variables (`{{$randomEmail}}` and the rest): a fresh value on every
//! use. The names and kinds of value follow Postman's list (Bruno's `faker-functions.ts`
//! maps them 1:1); the word lists are short and our own, since a faker crate would carry
//! megabytes of locale data for values nobody reads closely.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Name and what it gives, for autocomplete and the variables quick look.
pub const DYNAMIC: &[(&str, &str)] = &[
    ("$guid", "random UUID v4"),
    ("$randomUUID", "random UUID v4"),
    ("$timestamp", "Unix time, seconds"),
    ("$isoTimestamp", "current UTC time, ISO 8601"),
    ("$randomNanoId", "21-character URL-safe ID"),
    ("$randomAlphaNumeric", "one letter or digit"),
    ("$randomBoolean", "true or false"),
    ("$randomInt", "random integer 0-1000"),
    ("$randomColor", "colour name"),
    ("$randomHexColor", "#rrggbb"),
    ("$randomAbbreviation", "e.g. SQL, HTTP"),
    ("$randomIP", "IPv4 address"),
    ("$randomIPV4", "IPv4 address"),
    ("$randomIPV6", "IPv6 address"),
    ("$randomMACAddress", "MAC address"),
    ("$randomPassword", "15 letters and digits"),
    ("$randomLocale", "two-letter country code"),
    ("$randomUserAgent", "browser User-Agent"),
    ("$randomProtocol", "http or https"),
    ("$randomSemver", "x.y.z"),
    ("$randomFirstName", "first name"),
    ("$randomLastName", "last name"),
    ("$randomFullName", "first and last name"),
    ("$randomNamePrefix", "Mr., Ms., Dr. …"),
    ("$randomNameSuffix", "Jr., Sr., PhD …"),
    ("$randomJobArea", "e.g. Infrastructure"),
    ("$randomJobDescriptor", "e.g. Senior"),
    ("$randomJobTitle", "e.g. Senior Data Engineer"),
    ("$randomJobType", "e.g. Engineer"),
    ("$randomPhoneNumber", "phone number"),
    ("$randomPhoneNumberExt", "phone number with extension"),
    ("$randomCity", "city"),
    ("$randomStreetName", "street name"),
    ("$randomStreetAddress", "number and street"),
    ("$randomCountry", "country"),
    ("$randomCountryCode", "two-letter country code"),
    ("$randomLatitude", "latitude"),
    ("$randomLongitude", "longitude"),
    ("$randomAvatarImage", "avatar image URL"),
    ("$randomImageUrl", "image URL"),
    ("$randomAbstractImage", "image URL"),
    ("$randomAnimalsImage", "image URL"),
    ("$randomBusinessImage", "image URL"),
    ("$randomCatsImage", "image URL"),
    ("$randomCityImage", "image URL"),
    ("$randomFoodImage", "image URL"),
    ("$randomNightlifeImage", "image URL"),
    ("$randomFashionImage", "image URL"),
    ("$randomPeopleImage", "image URL"),
    ("$randomNatureImage", "image URL"),
    ("$randomSportsImage", "image URL"),
    ("$randomTransportImage", "image URL"),
    ("$randomImageDataUri", "small SVG as a data: URI"),
    ("$randomBankAccount", "8-digit account number"),
    ("$randomBankAccountName", "e.g. Savings Account"),
    ("$randomCreditCardMask", "last 4 digits of a card"),
    ("$randomBankAccountBic", "BIC/SWIFT code"),
    ("$randomBankAccountIban", "IBAN with valid check digits"),
    ("$randomTransactionType", "deposit, withdrawal …"),
    ("$randomCurrencyCode", "e.g. EUR"),
    ("$randomCurrencyName", "e.g. Euro"),
    ("$randomCurrencySymbol", "e.g. €"),
    ("$randomBitcoin", "Bitcoin address"),
    ("$randomCompanyName", "company name"),
    ("$randomCompanySuffix", "Inc, LLC …"),
    ("$randomBs", "business phrase"),
    ("$randomBsAdjective", "business adjective"),
    ("$randomBsBuzz", "business verb"),
    ("$randomBsNoun", "business noun"),
    ("$randomCatchPhrase", "catch phrase"),
    ("$randomCatchPhraseAdjective", "catch-phrase adjective"),
    ("$randomCatchPhraseDescriptor", "catch-phrase descriptor"),
    ("$randomCatchPhraseNoun", "catch-phrase noun"),
    ("$randomDatabaseColumn", "column name"),
    ("$randomDatabaseType", "column type"),
    ("$randomDatabaseCollation", "collation"),
    ("$randomDatabaseEngine", "storage engine"),
    ("$randomDateFuture", "date within a year ahead, ISO 8601"),
    ("$randomDatePast", "date within the past year, ISO 8601"),
    ("$randomDateRecent", "date within the past day, ISO 8601"),
    ("$randomWeekday", "weekday"),
    ("$randomMonth", "month"),
    ("$randomDomainName", "domain name"),
    ("$randomDomainSuffix", "com, org …"),
    ("$randomDomainWord", "one domain label"),
    ("$randomEmail", "e-mail address"),
    ("$randomExampleEmail", "e-mail at example.com/net/org"),
    ("$randomUserName", "user name"),
    ("$randomUrl", "URL"),
    ("$randomFileName", "file name"),
    ("$randomFileType", "e.g. image"),
    ("$randomFileExt", "e.g. pdf"),
    ("$randomCommonFileName", "file name, common type"),
    ("$randomCommonFileType", "e.g. text"),
    ("$randomCommonFileExt", "e.g. png"),
    ("$randomFilePath", "absolute file path"),
    ("$randomDirectoryPath", "absolute directory path"),
    ("$randomMimeType", "MIME type"),
    ("$randomPrice", "price, two decimals"),
    ("$randomProduct", "product"),
    ("$randomProductAdjective", "product adjective"),
    ("$randomProductMaterial", "material"),
    ("$randomProductName", "product name"),
    ("$randomDepartment", "store department"),
    ("$randomNoun", "noun"),
    ("$randomVerb", "verb"),
    ("$randomIngverb", "verb ending in -ing"),
    ("$randomAdjective", "adjective"),
    ("$randomWord", "word"),
    ("$randomWords", "a few words"),
    ("$randomPhrase", "phrase"),
    ("$randomLoremWord", "lorem ipsum word"),
    ("$randomLoremWords", "3 lorem words"),
    ("$randomLoremSentence", "lorem sentence"),
    ("$randomLoremSentences", "2-6 lorem sentences"),
    ("$randomLoremParagraph", "lorem paragraph"),
    ("$randomLoremParagraphs", "3 lorem paragraphs"),
    ("$randomLoremText", "lorem text"),
    ("$randomLoremSlug", "lorem-words-slug"),
    ("$randomLoremLines", "1-5 lorem lines"),
];

const FIRST: &[&str] = &[
    "James", "Mary", "Wei", "Yuki", "Olivia", "Liam", "Sofia", "Noah", "Amelia", "Lucas", "Chloe",
    "Mateo", "Hana", "Ethan", "Isla", "Arjun", "Maya", "Leo", "Zoe", "Omar",
];
const LAST: &[&str] = &[
    "Smith", "Chen", "Garcia", "Tanaka", "Müller", "Johnson", "Lin", "Brown", "Kim", "Rossi",
    "Silva", "Novak", "Wang", "Dubois", "Patel", "Lee", "Martin", "Kowalski",
];
const PREFIX: &[&str] = &["Mr.", "Mrs.", "Ms.", "Miss", "Dr."];
const SUFFIX: &[&str] = &["Jr.", "Sr.", "I", "II", "III", "IV", "MD", "PhD", "DDS"];
const JOB_AREA: &[&str] = &[
    "Solutions",
    "Program",
    "Brand",
    "Security",
    "Research",
    "Marketing",
    "Directives",
    "Implementation",
    "Integration",
    "Functionality",
    "Response",
    "Paradigm",
    "Tactics",
    "Identity",
    "Markets",
    "Group",
    "Division",
    "Applications",
    "Optimization",
    "Operations",
    "Infrastructure",
    "Intranet",
    "Communications",
    "Web",
    "Branding",
    "Quality",
    "Assurance",
    "Mobility",
    "Accounts",
    "Factors",
    "Creative",
    "Configuration",
    "Accountability",
    "Interactions",
    "Usability",
    "Metrics",
];
const JOB_DESCRIPTOR: &[&str] = &[
    "Lead",
    "Senior",
    "Direct",
    "Corporate",
    "Dynamic",
    "Future",
    "Product",
    "National",
    "Regional",
    "District",
    "Central",
    "Global",
    "Customer",
    "Investor",
    "International",
    "Legacy",
    "Forward",
    "Internal",
    "Human",
    "Chief",
    "Principal",
];
const JOB_TYPE: &[&str] = &[
    "Supervisor",
    "Associate",
    "Executive",
    "Liaison",
    "Officer",
    "Manager",
    "Engineer",
    "Specialist",
    "Director",
    "Coordinator",
    "Administrator",
    "Architect",
    "Analyst",
    "Designer",
    "Planner",
    "Orchestrator",
    "Technician",
    "Developer",
    "Producer",
    "Consultant",
    "Assistant",
    "Facilitator",
    "Agent",
    "Representative",
    "Strategist",
];
const CITY: &[&str] = &[
    "Taipei",
    "Tokyo",
    "Berlin",
    "Lisbon",
    "Austin",
    "Toronto",
    "Seoul",
    "Lyon",
    "Osaka",
    "Melbourne",
    "Denver",
    "Kraków",
    "Porto",
    "Kaohsiung",
    "Oslo",
    "Dublin",
    "Nagoya",
];
const STREET: &[&str] = &[
    "Main Street",
    "Oak Avenue",
    "Maple Drive",
    "Cedar Lane",
    "Park Road",
    "Elm Street",
    "Lake View",
    "Hill Street",
    "River Road",
    "Sunset Boulevard",
    "Station Road",
];
const COUNTRY: &[(&str, &str)] = &[
    ("Taiwan", "TW"),
    ("Japan", "JP"),
    ("Germany", "DE"),
    ("France", "FR"),
    ("United States", "US"),
    ("Canada", "CA"),
    ("Brazil", "BR"),
    ("India", "IN"),
    ("Australia", "AU"),
    ("South Korea", "KR"),
    ("Italy", "IT"),
    ("Spain", "ES"),
    ("Netherlands", "NL"),
    ("Sweden", "SE"),
    ("Poland", "PL"),
    ("Portugal", "PT"),
    ("Mexico", "MX"),
    ("Ireland", "IE"),
    ("Norway", "NO"),
    ("Singapore", "SG"),
];
const COLOR: &[&str] = &[
    "red",
    "green",
    "blue",
    "yellow",
    "purple",
    "orange",
    "pink",
    "teal",
    "lime",
    "cyan",
    "magenta",
    "maroon",
    "olive",
    "navy",
    "silver",
    "gold",
    "indigo",
    "violet",
    "salmon",
    "turquoise",
    "orchid",
    "plum",
    "tan",
    "ivory",
    "azure",
    "white",
    "black",
    "grey",
];
const HACKER_ABBR: &[&str] = &[
    "ADP", "AGP", "AI", "API", "ASCII", "CLI", "COM", "CSS", "DNS", "EXE", "FTP", "GB", "HDD",
    "HEX", "HTTP", "IB", "IP", "JBOD", "JSON", "OCR", "PCI", "PNG", "RAM", "RSS", "SAS", "SCSI",
    "SDD", "SMS", "SMTP", "SQL", "SSD", "SSL", "TCP", "THX", "TLS", "UDP", "USB", "UTF8", "VGA",
    "XML", "XSS",
];
const HACKER_ADJ: &[&str] = &[
    "auxiliary",
    "primary",
    "back-end",
    "digital",
    "open-source",
    "virtual",
    "cross-platform",
    "redundant",
    "online",
    "haptic",
    "multi-byte",
    "bluetooth",
    "wireless",
    "1080p",
    "neural",
    "optical",
    "solid state",
    "mobile",
];
const HACKER_NOUN: &[&str] = &[
    "driver",
    "protocol",
    "bandwidth",
    "panel",
    "microchip",
    "program",
    "port",
    "card",
    "array",
    "interface",
    "system",
    "sensor",
    "firewall",
    "hard drive",
    "pixel",
    "alarm",
    "feed",
    "monitor",
    "application",
    "transmitter",
    "bus",
    "circuit",
    "capacitor",
    "matrix",
];
const HACKER_VERB: &[&str] = &[
    "back up",
    "bypass",
    "hack",
    "override",
    "compress",
    "copy",
    "navigate",
    "index",
    "connect",
    "generate",
    "quantify",
    "calculate",
    "synthesize",
    "input",
    "transmit",
    "program",
    "reboot",
    "parse",
];
const HACKER_ING: &[&str] = &[
    "backing up",
    "bypassing",
    "hacking",
    "overriding",
    "compressing",
    "copying",
    "navigating",
    "indexing",
    "connecting",
    "generating",
    "quantifying",
    "calculating",
    "synthesizing",
    "transmitting",
    "programming",
    "parsing",
];
const LOREM: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
    "enim",
    "ad",
    "minim",
    "veniam",
    "quis",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "nisi",
    "aliquip",
    "ex",
    "ea",
    "commodo",
    "consequat",
    "duis",
    "aute",
    "irure",
    "in",
    "reprehenderit",
    "voluptate",
    "velit",
    "esse",
    "cillum",
    "fugiat",
    "nulla",
    "pariatur",
    "excepteur",
    "sint",
    "occaecat",
    "cupidatat",
    "non",
    "proident",
    "sunt",
    "culpa",
    "qui",
    "officia",
    "deserunt",
    "mollit",
    "anim",
    "id",
    "est",
    "laborum",
];
const COMPANY_SUFFIX: &[&str] = &["Inc", "and Sons", "LLC", "Group", "Ltd", "GmbH", "Co"];
const BS_ADJ: &[&str] = &[
    "clicks-and-mortar",
    "value-added",
    "vertical",
    "proactive",
    "robust",
    "revolutionary",
    "scalable",
    "leading-edge",
    "innovative",
    "intuitive",
    "strategic",
    "e-business",
    "mission-critical",
    "sticky",
    "one-to-one",
    "24/7",
    "end-to-end",
    "global",
    "seamless",
];
const BS_BUZZ: &[&str] = &[
    "synergize",
    "strategize",
    "empower",
    "leverage",
    "envisioneer",
    "monetize",
    "harness",
    "facilitate",
    "seize",
    "disintermediate",
    "integrate",
    "streamline",
    "optimize",
    "evolve",
    "transform",
    "embrace",
    "enable",
    "orchestrate",
    "reinvent",
    "aggregate",
];
const BS_NOUN: &[&str] = &[
    "synergies",
    "paradigms",
    "markets",
    "partnerships",
    "infrastructures",
    "platforms",
    "initiatives",
    "channels",
    "communities",
    "solutions",
    "e-markets",
    "action-items",
    "portals",
    "niches",
    "technologies",
    "content",
    "supply-chains",
    "convergence",
    "relationships",
    "architectures",
    "interfaces",
    "metrics",
    "web services",
];
const CP_ADJ: &[&str] = &[
    "Adaptive",
    "Advanced",
    "Automated",
    "Balanced",
    "Centralized",
    "Compatible",
    "Configurable",
    "Cross-group",
    "Customizable",
    "Decentralized",
    "Digitized",
    "Distributed",
    "Diverse",
    "Ergonomic",
    "Exclusive",
    "Expanded",
    "Focused",
    "Fully-configurable",
    "Integrated",
    "Intuitive",
    "Managed",
    "Multi-layered",
    "Networked",
    "Optimized",
    "Persistent",
    "Proactive",
    "Reactive",
    "Robust",
    "Seamless",
    "Secured",
    "Sharable",
    "Synchronized",
    "Total",
    "Universal",
    "User-friendly",
    "Versatile",
    "Virtual",
];
const CP_DESCRIPTOR: &[&str] = &[
    "24 hour",
    "24/7",
    "3rd generation",
    "4th generation",
    "actuating",
    "analyzing",
    "asymmetric",
    "asynchronous",
    "background",
    "bi-directional",
    "client-driven",
    "coherent",
    "contextually-based",
    "dedicated",
    "demand-driven",
    "dynamic",
    "encompassing",
    "explicit",
    "global",
    "heuristic",
    "high-level",
    "holistic",
    "interactive",
    "local",
    "logistical",
    "mission-critical",
    "modular",
    "multimedia",
    "next generation",
    "real-time",
    "responsive",
    "scalable",
    "stable",
    "static",
    "systematic",
    "tangible",
    "transitional",
    "zero tolerance",
];
const CP_NOUN: &[&str] = &[
    "ability",
    "access",
    "adapter",
    "algorithm",
    "alliance",
    "analyzer",
    "application",
    "approach",
    "architecture",
    "archive",
    "array",
    "attitude",
    "benchmark",
    "capability",
    "challenge",
    "circuit",
    "collaboration",
    "complexity",
    "concept",
    "database",
    "definition",
    "emulation",
    "encoding",
    "encryption",
    "firmware",
    "flexibility",
    "framework",
    "function",
    "hardware",
    "help-desk",
    "hierarchy",
    "hub",
    "implementation",
    "infrastructure",
    "initiative",
    "interface",
    "knowledge base",
    "matrix",
    "methodology",
    "middleware",
    "model",
    "moderator",
    "monitoring",
    "neural-net",
    "paradigm",
    "portal",
    "process improvement",
    "product",
    "protocol",
    "service-desk",
    "software",
    "solution",
    "standardization",
    "strategy",
    "structure",
    "system engine",
    "task-force",
    "throughput",
    "toolset",
    "website",
    "workforce",
];
const DB_COLUMN: &[&str] = &[
    "id",
    "title",
    "name",
    "email",
    "phone",
    "token",
    "group",
    "category",
    "password",
    "comment",
    "avatar",
    "status",
    "createdAt",
    "updatedAt",
];
const DB_TYPE: &[&str] = &[
    "int",
    "varchar",
    "text",
    "date",
    "datetime",
    "tinyint",
    "time",
    "timestamp",
    "smallint",
    "mediumint",
    "bigint",
    "decimal",
    "float",
    "double",
    "real",
    "bit",
    "boolean",
    "serial",
    "blob",
    "binary",
    "enum",
    "set",
    "geometry",
    "point",
];
const DB_COLLATION: &[&str] = &[
    "utf8_unicode_ci",
    "utf8_general_ci",
    "utf8_bin",
    "ascii_bin",
    "ascii_general_ci",
    "cp1250_bin",
    "cp1250_general_ci",
];
const DB_ENGINE: &[&str] = &["InnoDB", "MyISAM", "MEMORY", "CSV", "BLACKHOLE", "ARCHIVE"];
const WEEKDAY: &[&str] = &[
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const MONTH: &[&str] = &[
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DOMAIN_SUFFIX: &[&str] = &["com", "net", "org", "io", "info", "biz", "name", "dev"];
const FILE_TYPE: &[(&str, &[&str])] = &[
    ("application", &["pdf", "json", "zip", "xml", "gz"]),
    ("audio", &["mp3", "wav", "ogg"]),
    ("image", &["png", "jpeg", "gif", "webp", "svg"]),
    ("text", &["txt", "csv", "html", "css"]),
    ("video", &["mp4", "webm", "mov"]),
];
const COMMON_FILE: &[(&str, &str, &str)] = &[
    ("application", "pdf", "application/pdf"),
    ("audio", "mp3", "audio/mpeg"),
    ("image", "png", "image/png"),
    ("image", "jpeg", "image/jpeg"),
    ("image", "gif", "image/gif"),
    ("text", "html", "text/html"),
    ("text", "txt", "text/plain"),
    ("video", "mp4", "video/mp4"),
    ("application", "json", "application/json"),
];
const MIME: &[&str] = &[
    "application/json",
    "application/xml",
    "application/pdf",
    "application/zip",
    "application/octet-stream",
    "text/plain",
    "text/html",
    "text/csv",
    "image/png",
    "image/jpeg",
    "image/svg+xml",
    "audio/mpeg",
    "video/mp4",
    "multipart/form-data",
];
const PRODUCT: &[&str] = &[
    "Chair", "Car", "Computer", "Keyboard", "Mouse", "Bike", "Ball", "Gloves", "Pants", "Shirt",
    "Table", "Shoes", "Hat", "Towels", "Soap", "Tuna", "Chicken", "Fish", "Cheese", "Bacon",
    "Pizza", "Salad", "Sausages", "Chips",
];
const PRODUCT_ADJ: &[&str] = &[
    "Small",
    "Ergonomic",
    "Rustic",
    "Intelligent",
    "Gorgeous",
    "Incredible",
    "Fantastic",
    "Practical",
    "Sleek",
    "Awesome",
    "Generic",
    "Handcrafted",
    "Handmade",
    "Licensed",
    "Refined",
    "Unbranded",
    "Tasty",
];
const MATERIAL: &[&str] = &[
    "Steel", "Wooden", "Concrete", "Plastic", "Cotton", "Granite", "Rubber", "Metal", "Soft",
    "Fresh", "Frozen",
];
const DEPARTMENT: &[&str] = &[
    "Books",
    "Movies",
    "Music",
    "Games",
    "Electronics",
    "Computers",
    "Home",
    "Garden",
    "Tools",
    "Grocery",
    "Health",
    "Beauty",
    "Toys",
    "Kids",
    "Baby",
    "Clothing",
    "Shoes",
    "Jewelry",
    "Sports",
    "Outdoors",
    "Automotive",
    "Industrial",
];
const ACCOUNT_NAME: &[&str] = &[
    "Checking Account",
    "Savings Account",
    "Money Market Account",
    "Investment Account",
    "Home Loan Account",
    "Credit Card Account",
    "Auto Loan Account",
    "Personal Loan Account",
];
const TRANSACTION: &[&str] = &["deposit", "withdrawal", "payment", "invoice"];
const CURRENCY: &[(&str, &str, &str)] = &[
    ("USD", "US Dollar", "$"),
    ("EUR", "Euro", "€"),
    ("JPY", "Yen", "¥"),
    ("GBP", "Pound Sterling", "£"),
    ("TWD", "New Taiwan Dollar", "NT$"),
    ("CHF", "Swiss Franc", "CHF"),
    ("KRW", "Won", "₩"),
    ("INR", "Indian Rupee", "₹"),
    ("CAD", "Canadian Dollar", "$"),
    ("AUD", "Australian Dollar", "$"),
];
const USER_AGENT: &[&str] = &[
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15",
    "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36 Edg/129.0.0.0",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Mobile/15E148 Safari/604.1",
];
const IMAGE_CATEGORY: &[(&str, &str)] = &[
    ("$randomAbstractImage", "abstract"),
    ("$randomAnimalsImage", "animals"),
    ("$randomBusinessImage", "business"),
    ("$randomCatsImage", "cats"),
    ("$randomCityImage", "city"),
    ("$randomFoodImage", "food"),
    ("$randomNightlifeImage", "nightlife"),
    ("$randomFashionImage", "fashion"),
    ("$randomPeopleImage", "people"),
    ("$randomNatureImage", "nature"),
    ("$randomSportsImage", "sports"),
    ("$randomTransportImage", "transport"),
];
const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const BASE58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("OS random source");
    u64::from_le_bytes(b)
}

/// 0..n (n > 0). The modulo bias is irrelevant for test data.
fn below(n: u64) -> u64 {
    u64() % n
}

fn range(lo: u64, hi: u64) -> u64 {
    lo + below(hi - lo + 1)
}

fn pick<T: Copy>(xs: &[T]) -> T {
    xs[below(xs.len() as u64) as usize]
}

fn chars(set: &[u8], n: usize) -> String {
    (0..n).map(|_| pick(set) as char).collect()
}

fn digits(n: usize) -> String {
    chars(b"0123456789", n)
}

fn uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS random source");
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

fn words(list: &[&str], n: usize) -> String {
    (0..n).map(|_| pick(list)).collect::<Vec<_>>().join(" ")
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn sentence() -> String {
    format!("{}.", capitalised(&words(LOREM, range(4, 10) as usize)))
}

fn sentences(n: usize) -> String {
    (0..n).map(|_| sentence()).collect::<Vec<_>>().join(" ")
}

fn paragraph() -> String {
    sentences(3)
}

fn ipv4() -> String {
    let b = u64().to_le_bytes();
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

fn domain_word() -> String {
    format!("{}-{}", pick(LAST), pick(HACKER_NOUN))
        .to_lowercase()
        .replace(['ü', ' '], "")
}

fn user_name() -> String {
    format!("{}.{}{}", pick(FIRST), pick(LAST), below(100))
        .to_lowercase()
        .replace('ü', "u")
}

/// Account number + mod-97 check digits, so validators accept it (ISO 13616).
fn iban() -> String {
    let (country, bban) = ("DE", digits(18));
    let numeric: String = format!("{bban}{country}00")
        .chars()
        .map(|c| match c {
            'A'..='Z' => (c as u32 - 'A' as u32 + 10).to_string(),
            _ => c.to_string(),
        })
        .collect();
    let rem = numeric
        .bytes()
        .fold(0u32, |r, d| (r * 10 + (d - b'0') as u32) % 97);
    format!("{country}{:02}{bban}", 98 - rem)
}

fn date_offset(now: Duration, secs: i64) -> String {
    let t = now.as_secs() as i64 + secs;
    crate::model::iso8601(Duration::from_secs(t.max(0) as u64))
}

/// The value for `name` (with its `$`), or None when it isn't a dynamic variable.
pub fn value(name: &str) -> Option<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    if let Some((_, cat)) = IMAGE_CATEGORY.iter().find(|(n, _)| *n == name) {
        return Some(format!(
            "https://loremflickr.com/640/480/{cat}?lock={}",
            below(10_000)
        ));
    }
    Some(match name {
        "$guid" | "$randomUUID" => uuid(),
        "$timestamp" => now.as_secs().to_string(),
        "$isoTimestamp" => crate::model::iso8601(now),
        "$randomNanoId" => chars(
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-",
            21,
        ),
        "$randomAlphaNumeric" => chars(ALNUM, 1),
        "$randomBoolean" => (below(2) == 1).to_string(),
        "$randomInt" => below(1001).to_string(),
        "$randomColor" => pick(COLOR).into(),
        "$randomHexColor" => format!("#{:06x}", below(0x100_0000)),
        "$randomAbbreviation" => pick(HACKER_ABBR).into(),
        "$randomIP" | "$randomIPV4" => ipv4(),
        "$randomIPV6" => (0..8)
            .map(|_| format!("{:x}", below(0x1_0000)))
            .collect::<Vec<_>>()
            .join(":"),
        "$randomMACAddress" => (0..6)
            .map(|_| format!("{:02x}", below(256)))
            .collect::<Vec<_>>()
            .join(":"),
        "$randomPassword" => chars(ALNUM, 15),
        "$randomLocale" | "$randomCountryCode" => pick(COUNTRY).1.into(),
        "$randomUserAgent" => pick(USER_AGENT).into(),
        "$randomProtocol" => pick(&["http", "https"]).into(),
        "$randomSemver" => format!("{}.{}.{}", below(10), below(10), below(10)),
        "$randomFirstName" => pick(FIRST).into(),
        "$randomLastName" => pick(LAST).into(),
        "$randomFullName" => format!("{} {}", pick(FIRST), pick(LAST)),
        "$randomNamePrefix" => pick(PREFIX).into(),
        "$randomNameSuffix" => pick(SUFFIX).into(),
        "$randomJobArea" => pick(JOB_AREA).into(),
        "$randomJobDescriptor" => pick(JOB_DESCRIPTOR).into(),
        "$randomJobTitle" => format!(
            "{} {} {}",
            pick(JOB_DESCRIPTOR),
            pick(JOB_AREA),
            pick(JOB_TYPE)
        ),
        "$randomJobType" => pick(JOB_TYPE).into(),
        "$randomPhoneNumber" => format!("({}) {}-{}", range(200, 999), digits(3), digits(4)),
        "$randomPhoneNumberExt" => format!(
            "({}) {}-{} x{}",
            range(200, 999),
            digits(3),
            digits(4),
            digits(3)
        ),
        "$randomCity" => pick(CITY).into(),
        "$randomStreetName" => pick(STREET).into(),
        "$randomStreetAddress" => format!("{} {}", range(1, 9999), pick(STREET)),
        "$randomCountry" => pick(COUNTRY).0.into(),
        "$randomLatitude" => format!("{:.4}", below(1_800_001) as f64 / 10_000.0 - 90.0),
        "$randomLongitude" => format!("{:.4}", below(3_600_001) as f64 / 10_000.0 - 180.0),
        "$randomAvatarImage" => format!(
            "https://avatars.githubusercontent.com/u/{}",
            range(1, 99_999_999)
        ),
        "$randomImageUrl" => format!("https://picsum.photos/seed/{}/640/480", below(10_000)),
        "$randomImageDataUri" => format!(
            "data:image/svg+xml;charset=UTF-8,%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%20width%3D%22640%22%20height%3D%22480%22%3E%3Crect%20width%3D%22100%25%22%20height%3D%22100%25%22%20fill%3D%22%23{:06x}%22%2F%3E%3C%2Fsvg%3E",
            below(0x100_0000)
        ),
        "$randomBankAccount" => digits(8),
        "$randomBankAccountName" => pick(ACCOUNT_NAME).into(),
        "$randomCreditCardMask" => digits(4),
        "$randomBankAccountBic" => format!(
            "{}{}{}",
            chars(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ", 4),
            pick(COUNTRY).1,
            chars(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789", 2)
        ),
        "$randomBankAccountIban" => iban(),
        "$randomTransactionType" => pick(TRANSACTION).into(),
        "$randomCurrencyCode" => pick(CURRENCY).0.into(),
        "$randomCurrencyName" => pick(CURRENCY).1.into(),
        "$randomCurrencySymbol" => pick(CURRENCY).2.into(),
        "$randomBitcoin" => format!("1{}", chars(BASE58, range(25, 33) as usize)),
        "$randomCompanyName" => format!("{} {}", pick(LAST), pick(COMPANY_SUFFIX)),
        "$randomCompanySuffix" => pick(COMPANY_SUFFIX).into(),
        "$randomBs" => format!("{} {} {}", pick(BS_BUZZ), pick(BS_ADJ), pick(BS_NOUN)),
        "$randomBsAdjective" => pick(BS_ADJ).into(),
        "$randomBsBuzz" => pick(BS_BUZZ).into(),
        "$randomBsNoun" => pick(BS_NOUN).into(),
        "$randomCatchPhrase" => {
            format!("{} {} {}", pick(CP_ADJ), pick(CP_DESCRIPTOR), pick(CP_NOUN))
        }
        "$randomCatchPhraseAdjective" => pick(CP_ADJ).into(),
        "$randomCatchPhraseDescriptor" => pick(CP_DESCRIPTOR).into(),
        "$randomCatchPhraseNoun" => pick(CP_NOUN).into(),
        "$randomDatabaseColumn" => pick(DB_COLUMN).into(),
        "$randomDatabaseType" => pick(DB_TYPE).into(),
        "$randomDatabaseCollation" => pick(DB_COLLATION).into(),
        "$randomDatabaseEngine" => pick(DB_ENGINE).into(),
        "$randomDateFuture" => date_offset(now, range(60, 365 * 86_400) as i64),
        "$randomDatePast" => date_offset(now, -(range(60, 365 * 86_400) as i64)),
        "$randomDateRecent" => date_offset(now, -(range(1, 86_400) as i64)),
        "$randomWeekday" => pick(WEEKDAY).into(),
        "$randomMonth" => pick(MONTH).into(),
        "$randomDomainName" => format!("{}.{}", domain_word(), pick(DOMAIN_SUFFIX)),
        "$randomDomainSuffix" => pick(DOMAIN_SUFFIX).into(),
        "$randomDomainWord" => domain_word(),
        "$randomEmail" => format!(
            "{}@{}",
            user_name(),
            pick(&["gmail.com", "yahoo.com", "hotmail.com", "outlook.com"])
        ),
        "$randomExampleEmail" => format!(
            "{}@{}",
            user_name(),
            pick(&["example.com", "example.net", "example.org"])
        ),
        "$randomUserName" => user_name(),
        "$randomUrl" => format!("https://{}.{}", domain_word(), pick(DOMAIN_SUFFIX)),
        "$randomFileName" => {
            let (_, exts) = pick(FILE_TYPE);
            format!("{}.{}", words(LOREM, 2).replace(' ', "_"), pick(exts))
        }
        "$randomFileType" => pick(FILE_TYPE).0.into(),
        "$randomFileExt" => pick(pick(FILE_TYPE).1).into(),
        "$randomCommonFileName" => {
            format!(
                "{}.{}",
                words(LOREM, 2).replace(' ', "_"),
                pick(COMMON_FILE).1
            )
        }
        "$randomCommonFileType" => pick(COMMON_FILE).0.into(),
        "$randomCommonFileExt" => pick(COMMON_FILE).1.into(),
        "$randomFilePath" => format!(
            "/{}/{}.{}",
            words(LOREM, 2).replace(' ', "/"),
            pick(LOREM),
            pick(COMMON_FILE).1
        ),
        "$randomDirectoryPath" => format!("/{}", words(LOREM, 3).replace(' ', "/")),
        "$randomMimeType" => pick(MIME).into(),
        "$randomPrice" => format!("{}.{:02}", range(1, 999), below(100)),
        "$randomProduct" => pick(PRODUCT).into(),
        "$randomProductAdjective" => pick(PRODUCT_ADJ).into(),
        "$randomProductMaterial" => pick(MATERIAL).into(),
        "$randomProductName" => {
            format!("{} {} {}", pick(PRODUCT_ADJ), pick(MATERIAL), pick(PRODUCT))
        }
        "$randomDepartment" => pick(DEPARTMENT).into(),
        "$randomNoun" | "$randomWord" => pick(HACKER_NOUN).into(),
        "$randomVerb" => pick(HACKER_VERB).into(),
        "$randomIngverb" => pick(HACKER_ING).into(),
        "$randomAdjective" => pick(HACKER_ADJ).into(),
        "$randomWords" | "$randomLoremWords" => words(LOREM, 3),
        "$randomPhrase" => format!(
            "If we {} the {}, we can get to the {} {} through the {} {}!",
            pick(HACKER_VERB),
            pick(HACKER_NOUN),
            pick(HACKER_ABBR),
            pick(HACKER_NOUN),
            pick(HACKER_ADJ),
            pick(HACKER_NOUN)
        ),
        "$randomLoremWord" => pick(LOREM).into(),
        "$randomLoremSentence" => sentence(),
        "$randomLoremSentences" => sentences(range(2, 6) as usize),
        "$randomLoremParagraph" | "$randomLoremText" => paragraph(),
        "$randomLoremParagraphs" => (0..3)
            .map(|_| paragraph())
            .collect::<Vec<_>>()
            .join("\n \r"),
        "$randomLoremSlug" => words(LOREM, 3).replace(' ', "-"),
        "$randomLoremLines" => (0..range(1, 5))
            .map(|_| sentence())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_has_a_value_and_nothing_else_does() {
        // The list drives autocomplete and "defined" colouring: a name there without a
        // generator would show green and then go out verbatim.
        for (name, _) in DYNAMIC {
            let v = value(name).unwrap_or_else(|| panic!("{name} has no generator"));
            assert!(!v.is_empty(), "{name}");
        }
        assert_eq!(value("$randomNope"), None);
        assert_eq!(value("randomEmail"), None, "the $ is part of the name");
    }

    #[test]
    fn values_have_the_shapes_servers_validate() {
        let email = value("$randomEmail").unwrap();
        let (local, host) = email.split_once('@').unwrap();
        assert!(!local.is_empty() && host.contains('.'), "{email}");
        assert!(email.is_ascii(), "{email}");
        // A server rejects a bad IBAN checksum: ISO 13616 mod 97 must give 1.
        for _ in 0..50 {
            let iban = value("$randomBankAccountIban").unwrap();
            let moved = format!("{}{}", &iban[4..], &iban[..4]);
            let rem = moved
                .chars()
                .flat_map(|c| match c {
                    'A'..='Z' => (c as u32 - 55).to_string().into_bytes(),
                    _ => vec![c as u8],
                })
                .fold(0u32, |r, d| (r * 10 + (d - b'0') as u32) % 97);
            assert_eq!(rem, 1, "{iban}");
        }
        assert!(
            value("$randomIPV4")
                .unwrap()
                .parse::<std::net::Ipv4Addr>()
                .is_ok()
        );
        assert!(
            value("$randomIPV6")
                .unwrap()
                .parse::<std::net::Ipv6Addr>()
                .is_ok()
        );
        let past = value("$randomDatePast").unwrap();
        let future = value("$randomDateFuture").unwrap();
        let now = value("$isoTimestamp").unwrap();
        assert!(past < now && now < future, "{past} {now} {future}");
        assert_eq!(value("$randomNanoId").unwrap().len(), 21);
        assert!(matches!(
            value("$randomBoolean").unwrap().as_str(),
            "true" | "false"
        ));
        let lat: f64 = value("$randomLatitude").unwrap().parse().unwrap();
        assert!((-90.0..=90.0).contains(&lat));
    }
}
