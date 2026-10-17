-- A subset of Northwind (Microsoft's sample database, MS-PL; see
-- ../README.md): the dimensions a sales dashboard needs. Orders aren't
-- here: they arrive as events, from the publisher, through Kayak.
--
-- Kayak reads each table whole every few seconds and writes it into
-- Tidepool; rows that didn't change cost nothing there. Edit one here and
-- the dashboards follow, for example:
--
--   UPDATE products SET category_id = 1 WHERE product_name = 'Chai';

CREATE TABLE categories (
    category_id   smallint PRIMARY KEY,
    category_name text NOT NULL,
    description   text
);

CREATE TABLE suppliers (
    supplier_id  smallint PRIMARY KEY,
    company_name text NOT NULL,
    contact_name text,
    city         text,
    country      text NOT NULL
);

CREATE TABLE products (
    product_id        smallint PRIMARY KEY,
    product_name      text NOT NULL,
    supplier_id       smallint NOT NULL REFERENCES suppliers,
    category_id       smallint NOT NULL REFERENCES categories,
    quantity_per_unit text,
    unit_price        numeric(10,2) NOT NULL,
    discontinued      boolean NOT NULL
);

CREATE TABLE customers (
    customer_id  text PRIMARY KEY,
    company_name text NOT NULL,
    contact_name text,
    city         text,
    region       text,
    country      text NOT NULL
);

CREATE TABLE employees (
    employee_id smallint PRIMARY KEY,
    first_name  text NOT NULL,
    last_name   text NOT NULL,
    title       text NOT NULL,
    city        text,
    country     text NOT NULL,
    reports_to  smallint REFERENCES employees
);

CREATE TABLE shippers (
    shipper_id   smallint PRIMARY KEY,
    company_name text NOT NULL
);

COPY categories FROM stdin;
1	Beverages	Soft drinks, coffees, teas, beers, and ales
2	Condiments	Sweet and savory sauces, relishes, spreads, and seasonings
3	Confections	Desserts, candies, and sweet breads
4	Dairy Products	Cheeses
5	Grains/Cereals	Breads, crackers, pasta, and cereal
6	Meat/Poultry	Prepared meats
7	Produce	Dried fruit and bean curd
8	Seafood	Seaweed and fish
\.

COPY suppliers FROM stdin;
1	Exotic Liquids	Charlotte Cooper	London	UK
2	New Orleans Cajun Delights	Shelley Burke	New Orleans	USA
3	Grandma Kelly's Homestead	Regina Murphy	Ann Arbor	USA
4	Tokyo Traders	Yoshi Nagase	Tokyo	Japan
5	Cooperativa de Quesos 'Las Cabras'	Antonio del Valle Saavedra	Oviedo	Spain
6	Mayumi's	Mayumi Ohno	Osaka	Japan
7	Pavlova, Ltd.	Ian Devling	Melbourne	Australia
8	Specialty Biscuits, Ltd.	Peter Wilson	Manchester	UK
9	PB Knäckebröd AB	Lars Peterson	Göteborg	Sweden
10	Refrescos Americanas LTDA	Carlos Diaz	Sao Paulo	Brazil
11	Heli Süßwaren GmbH & Co. KG	Petra Winkler	Berlin	Germany
12	Plutzer Lebensmittelgroßmärkte AG	Martin Bein	Frankfurt	Germany
13	Nord-Ost-Fisch Handelsgesellschaft mbH	Sven Petersen	Cuxhaven	Germany
14	Formaggi Fortini s.r.l.	Elio Rossi	Ravenna	Italy
15	Norske Meierier	Beate Vileid	Sandvika	Norway
16	Bigfoot Breweries	Cheryl Saylor	Bend	USA
17	Svensk Sjöföda AB	Michael Björn	Stockholm	Sweden
18	Aux joyeux ecclésiastiques	Guylène Nodier	Paris	France
19	New England Seafood Cannery	Robb Merchant	Boston	USA
20	Leka Trading	Chandra Leka	Singapore	Singapore
21	Lyngbysild	Niels Petersen	Lyngby	Denmark
22	Zaanse Snoepfabriek	Dirk Luchte	Zaandam	Netherlands
23	Karkki Oy	Anne Heikkonen	Lappeenranta	Finland
24	G'day, Mate	Wendy Mackenzie	Sydney	Australia
25	Ma Maison	Jean-Guy Lauzon	Montréal	Canada
26	Pasta Buttini s.r.l.	Giovanni Giudici	Salerno	Italy
27	Escargots Nouveaux	Marie Delamare	Montceau	France
28	Gai pâturage	Eliane Noz	Annecy	France
29	Forêts d'érables	Chantal Goulet	Ste-Hyacinthe	Canada
\.

COPY products FROM stdin;
1	Chai	8	1	10 boxes x 30 bags	18.00	t
2	Chang	1	1	24 - 12 oz bottles	19.00	t
3	Aniseed Syrup	1	2	12 - 550 ml bottles	10.00	f
4	Chef Anton's Cajun Seasoning	2	2	48 - 6 oz jars	22.00	f
5	Chef Anton's Gumbo Mix	2	2	36 boxes	21.35	t
6	Grandma's Boysenberry Spread	3	2	12 - 8 oz jars	25.00	f
7	Uncle Bob's Organic Dried Pears	3	7	12 - 1 lb pkgs.	30.00	f
8	Northwoods Cranberry Sauce	3	2	12 - 12 oz jars	40.00	f
9	Mishi Kobe Niku	4	6	18 - 500 g pkgs.	97.00	t
10	Ikura	4	8	12 - 200 ml jars	31.00	f
11	Queso Cabrales	5	4	1 kg pkg.	21.00	f
12	Queso Manchego La Pastora	5	4	10 - 500 g pkgs.	38.00	f
13	Konbu	6	8	2 kg box	6.00	f
14	Tofu	6	7	40 - 100 g pkgs.	23.25	f
15	Genen Shouyu	6	2	24 - 250 ml bottles	13.00	f
16	Pavlova	7	3	32 - 500 g boxes	17.45	f
17	Alice Mutton	7	6	20 - 1 kg tins	39.00	t
18	Carnarvon Tigers	7	8	16 kg pkg.	62.50	f
19	Teatime Chocolate Biscuits	8	3	10 boxes x 12 pieces	9.20	f
20	Sir Rodney's Marmalade	8	3	30 gift boxes	81.00	f
21	Sir Rodney's Scones	8	3	24 pkgs. x 4 pieces	10.00	f
22	Gustaf's Knäckebröd	9	5	24 - 500 g pkgs.	21.00	f
23	Tunnbröd	9	5	12 - 250 g pkgs.	9.00	f
24	Guaraná Fantástica	10	1	12 - 355 ml cans	4.50	t
25	NuNuCa Nuß-Nougat-Creme	11	3	20 - 450 g glasses	14.00	f
26	Gumbär Gummibärchen	11	3	100 - 250 g bags	31.23	f
27	Schoggi Schokolade	11	3	100 - 100 g pieces	43.90	f
28	Rössle Sauerkraut	12	7	25 - 825 g cans	45.60	t
29	Thüringer Rostbratwurst	12	6	50 bags x 30 sausgs.	123.79	t
30	Nord-Ost Matjeshering	13	8	10 - 200 g glasses	25.89	f
31	Gorgonzola Telino	14	4	12 - 100 g pkgs	12.50	f
32	Mascarpone Fabioli	14	4	24 - 200 g pkgs.	32.00	f
33	Geitost	15	4	500 g	2.50	f
34	Sasquatch Ale	16	1	24 - 12 oz bottles	14.00	f
35	Steeleye Stout	16	1	24 - 12 oz bottles	18.00	f
36	Inlagd Sill	17	8	24 - 250 g  jars	19.00	f
37	Gravad lax	17	8	12 - 500 g pkgs.	26.00	f
38	Côte de Blaye	18	1	12 - 75 cl bottles	263.50	f
39	Chartreuse verte	18	1	750 cc per bottle	18.00	f
40	Boston Crab Meat	19	8	24 - 4 oz tins	18.40	f
41	Jack's New England Clam Chowder	19	8	12 - 12 oz cans	9.65	f
42	Singaporean Hokkien Fried Mee	20	5	32 - 1 kg pkgs.	14.00	t
43	Ipoh Coffee	20	1	16 - 500 g tins	46.00	f
44	Gula Malacca	20	2	20 - 2 kg bags	19.45	f
45	Rogede sild	21	8	1k pkg.	9.50	f
46	Spegesild	21	8	4 - 450 g glasses	12.00	f
47	Zaanse koeken	22	3	10 - 4 oz boxes	9.50	f
48	Chocolade	22	3	10 pkgs.	12.75	f
49	Maxilaku	23	3	24 - 50 g pkgs.	20.00	f
50	Valkoinen suklaa	23	3	12 - 100 g bars	16.25	f
51	Manjimup Dried Apples	24	7	50 - 300 g pkgs.	53.00	f
52	Filo Mix	24	5	16 - 2 kg boxes	7.00	f
53	Perth Pasties	24	6	48 pieces	32.80	t
54	Tourtière	25	6	16 pies	7.45	f
55	Pâté chinois	25	6	24 boxes x 2 pies	24.00	f
56	Gnocchi di nonna Alice	26	5	24 - 250 g pkgs.	38.00	f
57	Ravioli Angelo	26	5	24 - 250 g pkgs.	19.50	f
58	Escargots de Bourgogne	27	8	24 pieces	13.25	f
59	Raclette Courdavault	28	4	5 kg pkg.	55.00	f
60	Camembert Pierrot	28	4	15 - 300 g rounds	34.00	f
61	Sirop d'érable	29	2	24 - 500 ml bottles	28.50	f
62	Tarte au sucre	29	3	48 pies	49.30	f
63	Vegie-spread	7	2	15 - 625 g jars	43.90	f
64	Wimmers gute Semmelknödel	12	5	20 bags x 4 pieces	33.25	f
65	Louisiana Fiery Hot Pepper Sauce	2	2	32 - 8 oz bottles	21.05	f
66	Louisiana Hot Spiced Okra	2	2	24 - 8 oz jars	17.00	f
67	Laughing Lumberjack Lager	16	1	24 - 12 oz bottles	14.00	f
68	Scottish Longbreads	8	3	10 boxes x 8 pieces	12.50	f
69	Gudbrandsdalsost	15	4	10 kg pkg.	36.00	f
70	Outback Lager	7	1	24 - 355 ml bottles	15.00	f
71	Flotemysost	15	4	10 - 500 g pkgs.	21.50	f
72	Mozzarella di Giovanni	14	4	24 - 200 g pkgs.	34.80	f
73	Röd Kaviar	17	8	24 - 150 g jars	15.00	f
74	Longlife Tofu	4	7	5 kg pkg.	10.00	f
75	Rhönbräu Klosterbier	12	1	24 - 0.5 l bottles	7.75	f
76	Lakkalikööri	23	1	500 ml	18.00	f
77	Original Frankfurter grüne Soße	12	2	12 boxes	13.00	f
\.

COPY customers FROM stdin;
ALFKI	Alfreds Futterkiste	Maria Anders	Berlin	\N	Germany
ANATR	Ana Trujillo Emparedados y helados	Ana Trujillo	México D.F.	\N	Mexico
ANTON	Antonio Moreno Taquería	Antonio Moreno	México D.F.	\N	Mexico
AROUT	Around the Horn	Thomas Hardy	London	\N	UK
BERGS	Berglunds snabbköp	Christina Berglund	Luleå	\N	Sweden
BLAUS	Blauer See Delikatessen	Hanna Moos	Mannheim	\N	Germany
BLONP	Blondesddsl père et fils	Frédérique Citeaux	Strasbourg	\N	France
BOLID	Bólido Comidas preparadas	Martín Sommer	Madrid	\N	Spain
BONAP	Bon app'	Laurence Lebihan	Marseille	\N	France
BOTTM	Bottom-Dollar Markets	Elizabeth Lincoln	Tsawassen	BC	Canada
BSBEV	B's Beverages	Victoria Ashworth	London	\N	UK
CACTU	Cactus Comidas para llevar	Patricio Simpson	Buenos Aires	\N	Argentina
CENTC	Centro comercial Moctezuma	Francisco Chang	México D.F.	\N	Mexico
CHOPS	Chop-suey Chinese	Yang Wang	Bern	\N	Switzerland
COMMI	Comércio Mineiro	Pedro Afonso	Sao Paulo	SP	Brazil
CONSH	Consolidated Holdings	Elizabeth Brown	London	\N	UK
DRACD	Drachenblut Delikatessen	Sven Ottlieb	Aachen	\N	Germany
DUMON	Du monde entier	Janine Labrune	Nantes	\N	France
EASTC	Eastern Connection	Ann Devon	London	\N	UK
ERNSH	Ernst Handel	Roland Mendel	Graz	\N	Austria
FAMIA	Familia Arquibaldo	Aria Cruz	Sao Paulo	SP	Brazil
FISSA	FISSA Fabrica Inter. Salchichas S.A.	Diego Roel	Madrid	\N	Spain
FOLIG	Folies gourmandes	Martine Rancé	Lille	\N	France
FOLKO	Folk och fä HB	Maria Larsson	Bräcke	\N	Sweden
FRANK	Frankenversand	Peter Franken	München	\N	Germany
FRANR	France restauration	Carine Schmitt	Nantes	\N	France
FRANS	Franchi S.p.A.	Paolo Accorti	Torino	\N	Italy
FURIB	Furia Bacalhau e Frutos do Mar	Lino Rodriguez	Lisboa	\N	Portugal
GALED	Galería del gastrónomo	Eduardo Saavedra	Barcelona	\N	Spain
GODOS	Godos Cocina Típica	José Pedro Freyre	Sevilla	\N	Spain
GOURL	Gourmet Lanchonetes	André Fonseca	Campinas	SP	Brazil
GREAL	Great Lakes Food Market	Howard Snyder	Eugene	OR	USA
GROSR	GROSELLA-Restaurante	Manuel Pereira	Caracas	DF	Venezuela
HANAR	Hanari Carnes	Mario Pontes	Rio de Janeiro	RJ	Brazil
HILAA	HILARION-Abastos	Carlos Hernández	San Cristóbal	Táchira	Venezuela
HUNGC	Hungry Coyote Import Store	Yoshi Latimer	Elgin	OR	USA
HUNGO	Hungry Owl All-Night Grocers	Patricia McKenna	Cork	Co. Cork	Ireland
ISLAT	Island Trading	Helen Bennett	Cowes	Isle of Wight	UK
KOENE	Königlich Essen	Philip Cramer	Brandenburg	\N	Germany
LACOR	La corne d'abondance	Daniel Tonini	Versailles	\N	France
LAMAI	La maison d'Asie	Annette Roulet	Toulouse	\N	France
LAUGB	Laughing Bacchus Wine Cellars	Yoshi Tannamuri	Vancouver	BC	Canada
LAZYK	Lazy K Kountry Store	John Steel	Walla Walla	WA	USA
LEHMS	Lehmanns Marktstand	Renate Messner	Frankfurt a.M.	\N	Germany
LETSS	Let's Stop N Shop	Jaime Yorres	San Francisco	CA	USA
LILAS	LILA-Supermercado	Carlos González	Barquisimeto	Lara	Venezuela
LINOD	LINO-Delicateses	Felipe Izquierdo	I. de Margarita	Nueva Esparta	Venezuela
LONEP	Lonesome Pine Restaurant	Fran Wilson	Portland	OR	USA
MAGAA	Magazzini Alimentari Riuniti	Giovanni Rovelli	Bergamo	\N	Italy
MAISD	Maison Dewey	Catherine Dewey	Bruxelles	\N	Belgium
MEREP	Mère Paillarde	Jean Fresnière	Montréal	Québec	Canada
MORGK	Morgenstern Gesundkost	Alexander Feuer	Leipzig	\N	Germany
NORTS	North/South	Simon Crowther	London	\N	UK
OCEAN	Océano Atlántico Ltda.	Yvonne Moncada	Buenos Aires	\N	Argentina
OLDWO	Old World Delicatessen	Rene Phillips	Anchorage	AK	USA
OTTIK	Ottilies Käseladen	Henriette Pfalzheim	Köln	\N	Germany
PARIS	Paris spécialités	Marie Bertrand	Paris	\N	France
PERIC	Pericles Comidas clásicas	Guillermo Fernández	México D.F.	\N	Mexico
PICCO	Piccolo und mehr	Georg Pipps	Salzburg	\N	Austria
PRINI	Princesa Isabel Vinhos	Isabel de Castro	Lisboa	\N	Portugal
QUEDE	Que Delícia	Bernardo Batista	Rio de Janeiro	RJ	Brazil
QUEEN	Queen Cozinha	Lúcia Carvalho	Sao Paulo	SP	Brazil
QUICK	QUICK-Stop	Horst Kloss	Cunewalde	\N	Germany
RANCH	Rancho grande	Sergio Gutiérrez	Buenos Aires	\N	Argentina
RATTC	Rattlesnake Canyon Grocery	Paula Wilson	Albuquerque	NM	USA
REGGC	Reggiani Caseifici	Maurizio Moroni	Reggio Emilia	\N	Italy
RICAR	Ricardo Adocicados	Janete Limeira	Rio de Janeiro	RJ	Brazil
RICSU	Richter Supermarkt	Michael Holz	Genève	\N	Switzerland
ROMEY	Romero y tomillo	Alejandra Camino	Madrid	\N	Spain
SANTG	Santé Gourmet	Jonas Bergulfsen	Stavern	\N	Norway
SAVEA	Save-a-lot Markets	Jose Pavarotti	Boise	ID	USA
SEVES	Seven Seas Imports	Hari Kumar	London	\N	UK
SIMOB	Simons bistro	Jytte Petersen	Kobenhavn	\N	Denmark
SPECD	Spécialités du monde	Dominique Perrier	Paris	\N	France
SPLIR	Split Rail Beer & Ale	Art Braunschweiger	Lander	WY	USA
SUPRD	Suprêmes délices	Pascale Cartrain	Charleroi	\N	Belgium
THEBI	The Big Cheese	Liz Nixon	Portland	OR	USA
THECR	The Cracker Box	Liu Wong	Butte	MT	USA
TOMSP	Toms Spezialitäten	Karin Josephs	Münster	\N	Germany
TORTU	Tortuga Restaurante	Miguel Angel Paolino	México D.F.	\N	Mexico
TRADH	Tradição Hipermercados	Anabela Domingues	Sao Paulo	SP	Brazil
TRAIH	Trail's Head Gourmet Provisioners	Helvetius Nagy	Kirkland	WA	USA
VAFFE	Vaffeljernet	Palle Ibsen	Århus	\N	Denmark
VICTE	Victuailles en stock	Mary Saveley	Lyon	\N	France
VINET	Vins et alcools Chevalier	Paul Henriot	Reims	\N	France
WANDK	Die Wandernde Kuh	Rita Müller	Stuttgart	\N	Germany
WARTH	Wartian Herkku	Pirkko Koskitalo	Oulu	\N	Finland
WELLI	Wellington Importadora	Paula Parente	Resende	SP	Brazil
WHITC	White Clover Markets	Karl Jablonski	Seattle	WA	USA
WILMK	Wilman Kala	Matti Karttunen	Helsinki	\N	Finland
WOLZA	Wolski  Zajazd	Zbyszek Piestrzeniewicz	Warszawa	\N	Poland
\.

COPY employees FROM stdin;
2	Andrew	Fuller	Vice President, Sales	Tacoma	USA	\N
1	Nancy	Davolio	Sales Representative	Seattle	USA	2
3	Janet	Leverling	Sales Representative	Kirkland	USA	2
4	Margaret	Peacock	Sales Representative	Redmond	USA	2
5	Steven	Buchanan	Sales Manager	London	UK	2
8	Laura	Callahan	Inside Sales Coordinator	Seattle	USA	2
6	Michael	Suyama	Sales Representative	London	UK	5
7	Robert	King	Sales Representative	London	UK	5
9	Anne	Dodsworth	Sales Representative	London	UK	5
\.

COPY shippers FROM stdin;
1	Speedy Express
2	United Package
3	Federal Shipping
4	Alliance Shippers
5	UPS
6	DHL
\.

