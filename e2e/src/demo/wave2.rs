//! The second wave of demo elections: every davinci-sdk ballot preset and a
//! few other shapes, every census origin and key mode, the organizer's
//! controls and the votes a node must refuse. Most are in English, Spanish
//! and Catalan.

use super::{
    Action, BallotKind, CensusKind, DAY, KeySource, Lang, Lifecycle, MINUTE, MetaPlan, Refusal,
    Revision, Route, Spec,
};

/// Wave-2 defaults: ballot modes as davinci-sdk builds them.
const W2: Spec = Spec {
    sdk: true,
    ..Spec::BASE
};

static FEE_CLARIFIED: Revision = Revision {
    description: "The association is setting next year's annual membership fee. Enter the \
        amount in euros, from 0 to 100, that you think is fair. The fee will be the average of all \
        answers, rounded to the nearest euro. Clarification added during the vote: the fee covers \
        the whole calendar year, and members under 25 pay half of the amount agreed.",
    i18n: &[
        "La asociación fija la cuota anual del año que viene. Escribe la cantidad en euros, de 0 \
        a 100, que te parezca justa. La cuota será la media de todas las respuestas, redondeada al \
        euro. Aclaración añadida durante la votación: la cuota cubre todo el año natural y los \
        socios menores de 25 años pagan la mitad de la cantidad acordada.",
        "L'associació fixa la quota anual de l'any vinent. Escriu la quantitat en euros, de 0 a \
        100, que et sembli justa. La quota serà la mitjana de totes les respostes, arrodonida a \
        l'euro. Aclariment afegit durant la votació: la quota cobreix tot l'any natural i els \
        socis menors de 25 anys paguen la meitat de la quantitat acordada.",
    ],
};

static LUNCHES_CORRECTED: Revision = Revision {
    description: "The community kitchen serves a shared lunch on the first Saturday of every \
        month. Choose the menu we cook most often next season; the others will rotate. Corrected \
        before voting opened: the lunches are monthly, not weekly.",
    i18n: &[
        "La cocina comunitaria sirve una comida compartida el primer sábado de cada mes. Elige el \
        menú que cocinaremos más a menudo la próxima temporada; los demás irán rotando. Corregido \
        antes de abrir la votación: las comidas son mensuales, no semanales.",
        "La cuina comunitària serveix un dinar compartit el primer dissabte de cada mes. Tria el \
        menú que cuinarem més sovint la temporada vinent; els altres aniran rotant. Corregit abans \
        d'obrir la votació: els dinars són mensuals, no setmanals.",
    ],
};

static WATERING_DRAFT: Revision = Revision {
    description: "The allotment gardens need a new watering schedule for the summer. Pick the \
        slot that suits you best.",
    i18n: &[
        "Los huertos urbanos necesitan un nuevo horario de riego para el verano. Elige la franja \
        que mejor te vaya.",
        "Els horts urbans necessiten un nou horari de reg per a l'estiu. Tria la franja que et \
        vagi millor.",
    ],
};

/// The second wave: about twenty elections that together take every ballot
/// kind, census origin, key mode, organizer action and refused vote.
pub fn elections() -> Vec<Spec> {
    vec![
        Spec {
            n: 1,
            dir: "wave2/1-neighbourhood-plan",
            title: "Neighbourhood plan 2027: which improvements do you support?",
            description: "The district council is drafting next year's neighbourhood plan and \
                wants to know which improvements residents back. Approve as many of the sixteen \
                proposals as you like; the most approved ones go into the plan and are funded \
                first.",
            question: "Which improvements do you support?",
            question_description: "Approve any number of proposals, or none.",
            choices: &[
                "More trees along the main avenue",
                "Safer crossings near the schools",
                "A covered playground in the central park",
                "Longer library opening hours",
                "Public drinking fountains",
                "Bins for organic waste on every street",
                "A weekly farmers' market in the square",
                "Shelters and benches at bus stops",
                "A protected cycle lane to the station",
                "Free wifi in public buildings",
                "A community vegetable garden",
                "Quieter streets on weekend nights",
                "Better street lighting in the old town",
                "An outdoor gym by the river",
                "A youth centre open in the evenings",
                "More frequent night buses",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Plan de barrio 2027: ¿qué mejoras apoyas?",
                    description: "El distrito está preparando el plan de barrio del año que \
                        viene y quiere saber qué mejoras apoyan los vecinos. Aprueba tantas de \
                        las dieciséis propuestas como quieras; las más aprobadas entrarán en el \
                        plan y se financiarán primero.",
                    question: "¿Qué mejoras apoyas?",
                    question_description: "Aprueba las propuestas que quieras, o ninguna.",
                    choices: &[
                        "Más árboles en la avenida principal",
                        "Pasos de peatones más seguros cerca de las escuelas",
                        "Un parque infantil cubierto en el parque central",
                        "Más horas de apertura en la biblioteca",
                        "Fuentes públicas de agua potable",
                        "Contenedores de orgánico en todas las calles",
                        "Un mercado semanal de productores en la plaza",
                        "Marquesinas y bancos en las paradas de autobús",
                        "Un carril bici protegido hasta la estación",
                        "Wifi gratuito en los edificios públicos",
                        "Un huerto comunitario",
                        "Calles más tranquilas las noches de fin de semana",
                        "Mejor alumbrado en el casco antiguo",
                        "Un gimnasio al aire libre junto al río",
                        "Un espacio joven abierto por las tardes",
                        "Autobuses nocturnos más frecuentes",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Pla de barri 2027: a quines millores dones suport?",
                    description: "El districte prepara el pla de barri de l'any vinent i vol \
                        saber a quines millores donen suport els veïns. Aprova tantes propostes \
                        de les setze com vulguis; les més aprovades entraran al pla i es \
                        finançaran primer.",
                    question: "A quines millores dones suport?",
                    question_description: "Aprova les propostes que vulguis, o cap.",
                    choices: &[
                        "Més arbres a l'avinguda principal",
                        "Passos de vianants més segurs a prop de les escoles",
                        "Un parc infantil cobert al parc central",
                        "Més hores d'obertura a la biblioteca",
                        "Fonts públiques d'aigua potable",
                        "Contenidors d'orgànica a tots els carrers",
                        "Un mercat setmanal de pagès a la plaça",
                        "Marquesines i bancs a les parades d'autobús",
                        "Un carril bici protegit fins a l'estació",
                        "Wifi gratuït als edificis públics",
                        "Un hort comunitari",
                        "Carrers més tranquils les nits de cap de setmana",
                        "Millor enllumenat al nucli antic",
                        "Un gimnàs a l'aire lliure a la vora del riu",
                        "Un espai jove obert a la tarda",
                        "Autobusos nocturns més freqüents",
                    ],
                },
            ],
            lean: &[
                0.62, 0.71, 0.38, 0.33, 0.29, 0.41, 0.36, 0.44, 0.47, 0.18, 0.31, 0.39, 0.52, 0.22,
                0.27, 0.35,
            ],
            ballot: BallotKind::ApproveAny,
            members: 410,
            key: KeySource::Node(0),
            lifecycle: Lifecycle::Tally,
            // Every first ballot to node 1, in chunks of 130. At 16 fields a
            // slot update takes 33 of a blob's 4096 cells, so two blobs hold
            // about 121 new votes with their silent refreshes: from the second
            // chunk on the node splits what is pending over several
            // transitions.
            round1: 390,
            round2: (390, 400),
            revotes: 60,
            same_node: 30,
            route: Route::One(0),
            chunk: 130,
            max_voters: 410,
            ..W2
        },
        Spec {
            n: 2,
            dir: "wave2/2-festival-closing-night",
            title: "Summer festival: who plays the closing night?",
            description: "The festival committee shortlisted four acts for the closing night. \
                Vote for one, or leave the ballot blank if none convinces you. Voting closes on \
                its own one hour after it opens, and the result is published automatically.",
            question: "Which act should close the festival?",
            question_description: "Pick one act, or leave the ballot blank.",
            choices: &[
                "The music school brass band",
                "A jazz quartet",
                "A folk-rock group",
                "The town choir with a chamber orchestra",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Fiesta mayor: ¿quién toca en la noche de clausura?",
                    description: "La comisión de fiestas ha preseleccionado cuatro propuestas \
                        para la noche de clausura. Vota una, o deja la papeleta en blanco si \
                        ninguna te convence. La votación se cierra sola una hora después de \
                        abrirse y el resultado se publica automáticamente.",
                    question: "¿Quién debería cerrar la fiesta?",
                    question_description: "Elige una propuesta o deja la papeleta en blanco.",
                    choices: &[
                        "La banda de la escuela de música",
                        "Un cuarteto de jazz",
                        "Un grupo de folk-rock",
                        "El coro municipal con una orquesta de cámara",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Festa major: qui toca a la nit de cloenda?",
                    description: "La comissió de festes ha preseleccionat quatre propostes per a \
                        la nit de cloenda. Vota'n una, o deixa la papereta en blanc si cap no et \
                        convenç. La votació es tanca sola una hora després d'obrir-se i el \
                        resultat es publica automàticament.",
                    question: "Qui hauria de tancar la festa?",
                    question_description: "Tria una proposta o deixa la papereta en blanc.",
                    choices: &[
                        "La banda de l'escola de música",
                        "Un quartet de jazz",
                        "Un grup de folk-rock",
                        "La coral municipal amb una orquestra de cambra",
                    ],
                },
            ],
            lean: &[0.31, 0.24, 0.33, 0.12],
            ballot: BallotKind::SingleChoice { abstain: true },
            members: 30,
            key: KeySource::Node(1),
            lifecycle: Lifecycle::Timed { secs: 60 * MINUTE },
            actions: &[
                Action::Refuse(Refusal::NotInCensus),
                Action::Refuse(Refusal::BadSignature),
                Action::Refuse(Refusal::ReusedVoteId),
                Action::Refuse(Refusal::AfterEnd),
            ],
            round1: 18,
            round2: (18, 26),
            revotes: 4,
            same_node: 4,
            max_voters: 30,
            ..W2
        },
        Spec {
            n: 3,
            dir: "wave2/3-federation-delegates",
            title: "Sports club: two delegates to the federation",
            description: "The club sends two delegates to the regional federation's assembly. \
                Choose exactly two of the five members who volunteered. Voting closes on its own \
                after an hour. The ballots are encrypted with a key held jointly by an \
                independent committee, which decrypts only the final count.",
            question: "Which two members should represent the club?",
            question_description: "Choose exactly two.",
            choices: &[
                "Núria Casals",
                "Pablo Martín",
                "Fatima El Idrissi",
                "Jordi Roca",
                "Emma Laurent",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Club deportivo: dos delegados para la federación",
                    description: "El club envía dos delegados a la asamblea de la federación \
                        territorial. Elige exactamente a dos de los cinco socios que se han \
                        presentado. La votación se cierra sola al cabo de una hora. Las \
                        papeletas se cifran con una clave que custodia de forma conjunta un \
                        comité independiente, que solo descifra el recuento final.",
                    question: "¿Qué dos socios deberían representar al club?",
                    question_description: "Elige exactamente dos.",
                    choices: &[
                        "Núria Casals",
                        "Pablo Martín",
                        "Fatima El Idrissi",
                        "Jordi Roca",
                        "Emma Laurent",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Club esportiu: dos delegats per a la federació",
                    description: "El club envia dos delegats a l'assemblea de la federació \
                        territorial. Tria exactament dos dels cinc socis que s'han presentat. La \
                        votació es tanca sola al cap d'una hora. Les paperetes es xifren amb \
                        una clau que custodia de manera conjunta un comitè independent, que \
                        només desxifra el recompte final.",
                    question: "Quins dos socis haurien de representar el club?",
                    question_description: "Tria'n exactament dos.",
                    choices: &[
                        "Núria Casals",
                        "Pablo Martín",
                        "Fatima El Idrissi",
                        "Jordi Roca",
                        "Emma Laurent",
                    ],
                },
            ],
            lean: &[0.28, 0.22, 0.25, 0.15, 0.10],
            ballot: BallotKind::MultipleChoice { min: 2, max: 2 },
            members: 30,
            key: KeySource::DkgAutomatic,
            lifecycle: Lifecycle::Timed { secs: 60 * MINUTE },
            actions: &[
                Action::Shorten(10 * MINUTE),
                Action::Refuse(Refusal::BreaksRules),
            ],
            round1: 16,
            round2: (16, 24),
            revotes: 3,
            max_voters: 30,
            ..W2
        },
        Spec {
            n: 4,
            dir: "wave2/4-after-school-activities",
            title: "Parents' association: new after-school activities",
            description: "The school can add up to three after-school activities next term. \
                Choose between one and three; the most requested ones start in January.",
            question: "Which activities would your family sign up for?",
            question_description: "Choose between one and three.",
            choices: &[
                "Robotics",
                "Chess",
                "Theatre",
                "Swimming",
                "Choir",
                "School garden",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "AMPA: nuevas actividades extraescolares",
                    description: "La escuela puede añadir hasta tres actividades extraescolares \
                        el próximo trimestre. Elige entre una y tres; las más pedidas empezarán \
                        en enero.",
                    question: "¿Qué actividades elegiría tu familia?",
                    question_description: "Elige entre una y tres.",
                    choices: &[
                        "Robótica",
                        "Ajedrez",
                        "Teatro",
                        "Natación",
                        "Coro",
                        "Huerto escolar",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "AFA: noves activitats extraescolars",
                    description: "L'escola pot afegir fins a tres activitats extraescolars el \
                        trimestre vinent. Tria'n entre una i tres; les més demanades començaran \
                        al gener.",
                    question: "Quines activitats triaria la teva família?",
                    question_description: "Tria'n entre una i tres.",
                    choices: &[
                        "Robòtica",
                        "Escacs",
                        "Teatre",
                        "Natació",
                        "Coral",
                        "Hort escolar",
                    ],
                },
            ],
            lean: &[0.30, 0.14, 0.20, 0.26, 0.08, 0.17],
            ballot: BallotKind::MultipleChoice { min: 1, max: 3 },
            members: 24,
            key: KeySource::Node(0),
            lifecycle: Lifecycle::Tally,
            actions: &[Action::Pause],
            round1: 14,
            round2: (14, 20),
            revotes: 3,
            same_node: 1,
            max_voters: 24,
            ..W2
        },
        Spec {
            n: 5,
            dir: "wave2/5-bus-routes",
            title: "Rate the new bus routes",
            description: "Four new bus routes ran on trial this autumn. Rate each one from 1 \
                (poor) to 5 (excellent). The transport commission holds part of the decryption \
                key and publishes it while the vote is still open, so the count is ready as soon \
                as voting ends.",
            question: "How would you rate each route?",
            question_description: "1 is poor, 5 is excellent.",
            choices: &[
                "Route 14: hospital – university",
                "Route 22: old town loop",
                "Route 31: industrial estate express",
                "Night route N4",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Valora las nuevas líneas de autobús",
                    description: "Este otoño se han probado cuatro nuevas líneas de autobús. \
                        Valora cada una de 1 (mala) a 5 (excelente). La comisión de transporte \
                        guarda parte de la clave de descifrado y la publica mientras la votación \
                        sigue abierta, para que el recuento esté listo en cuanto se cierre.",
                    question: "¿Qué nota le das a cada línea?",
                    question_description: "1 es mala, 5 es excelente.",
                    choices: &[
                        "Línea 14: hospital – universidad",
                        "Línea 22: circular del casco antiguo",
                        "Línea 31: exprés al polígono industrial",
                        "Línea nocturna N4",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Valora les noves línies d'autobús",
                    description: "Aquesta tardor s'han provat quatre noves línies d'autobús. \
                        Valora cadascuna d'1 (dolenta) a 5 (excel·lent). La comissió de \
                        transport guarda part de la clau de desxifratge i la publica mentre la \
                        votació encara és oberta, perquè el recompte estigui a punt tan bon punt \
                        es tanqui.",
                    question: "Quina nota poses a cada línia?",
                    question_description: "1 és dolenta, 5 és excel·lent.",
                    choices: &[
                        "Línia 14: hospital – universitat",
                        "Línia 22: circular del nucli antic",
                        "Línia 31: exprés al polígon industrial",
                        "Línia nocturna N4",
                    ],
                },
            ],
            lean: &[0.72, 0.55, 0.40, 0.66],
            ballot: BallotKind::RatingFrom { min: 1, max: 5 },
            members: 24,
            key: KeySource::DkgLockedEarly,
            lifecycle: Lifecycle::Tally,
            round1: 14,
            round2: (14, 20),
            revotes: 3,
            max_voters: 24,
            ..W2
        },
        Spec {
            n: 6,
            dir: "wave2/6-participatory-budget",
            title: "Participatory budget: share 100 points",
            description: "Share 100 points among the five shortlisted projects, with at most 50 \
                on any one of them. The points decide how the €200,000 participatory budget is \
                split. Voting closes on its own when the time is up.",
            question: "How many points do you give each project?",
            question_description: "Up to 100 points in total, at most 50 per project.",
            choices: &[
                "Repair the covered market roof",
                "Ramps at the town hall and the library",
                "Solar lighting in the parks",
                "A weekend bus to the hospital",
                "Grants for plastic-free shops",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Presupuestos participativos: reparte 100 puntos",
                    description: "Reparte 100 puntos entre los cinco proyectos finalistas, con \
                        un máximo de 50 por proyecto. Los puntos deciden cómo se reparten los \
                        200.000 € del presupuesto participativo. La votación se cierra sola \
                        cuando se acaba el plazo.",
                    question: "¿Cuántos puntos das a cada proyecto?",
                    question_description: "Hasta 100 puntos en total y 50 como máximo por \
                        proyecto.",
                    choices: &[
                        "Reparar la cubierta del mercado",
                        "Rampas en el ayuntamiento y la biblioteca",
                        "Alumbrado solar en los parques",
                        "Un autobús de fin de semana al hospital",
                        "Ayudas para comercios sin plástico",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Pressupostos participatius: reparteix 100 punts",
                    description: "Reparteix 100 punts entre els cinc projectes finalistes, amb \
                        un màxim de 50 per projecte. Els punts decideixen com es reparteixen els \
                        200.000 € del pressupost participatiu. La votació es tanca sola quan \
                        s'acaba el termini.",
                    question: "Quants punts dones a cada projecte?",
                    question_description: "Fins a 100 punts en total i 50 com a màxim per \
                        projecte.",
                    choices: &[
                        "Reparar la coberta del mercat",
                        "Rampes a l'ajuntament i a la biblioteca",
                        "Enllumenat solar als parcs",
                        "Un autobús de cap de setmana a l'hospital",
                        "Ajuts per a comerços sense plàstic",
                    ],
                },
            ],
            lean: &[0.25, 0.22, 0.20, 0.18, 0.15],
            ballot: BallotKind::Budget {
                total: 100,
                cap: 50,
            },
            members: 20,
            key: KeySource::Node(1),
            lifecycle: Lifecycle::Timed { secs: 45 * MINUTE },
            actions: &[Action::Extend(25 * MINUTE)],
            round1: 12,
            round2: (12, 17),
            revotes: 2,
            same_node: 1,
            round3: (17, 19),
            max_voters: 20,
            ..W2
        },
        Spec {
            n: 7,
            dir: "wave2/7-housing-cooperative",
            title: "Housing cooperative: priorities for 2027",
            description: "Each member has 100 credits for the cooperative's five priorities in \
                2027 and must spend at least 50 of them. Giving a priority n votes costs n² \
                credits, so a strong preference costs more than broad support. Membership is \
                recorded on-chain: new members can vote as soon as they join.",
            question: "How many votes do you give each priority?",
            question_description: "Spend between 50 and 100 credits; n votes cost n² credits.",
            choices: &[
                "Insulate the façades",
                "Solar panels on the roofs",
                "A lift in building B",
                "A bicycle storage room",
                "A shared laundry",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Cooperativa de vivienda: prioridades para 2027",
                    description: "Cada socio tiene 100 créditos para las cinco prioridades de la \
                        cooperativa en 2027 y debe gastar al menos 50. Dar n votos a una \
                        prioridad cuesta n² créditos, así que una preferencia fuerte cuesta más \
                        que un apoyo repartido. La lista de socios está registrada en la cadena: \
                        los nuevos socios pueden votar en cuanto se incorporan.",
                    question: "¿Cuántos votos das a cada prioridad?",
                    question_description: "Gasta entre 50 y 100 créditos; n votos cuestan n² \
                        créditos.",
                    choices: &[
                        "Aislar las fachadas",
                        "Placas solares en las azoteas",
                        "Un ascensor en el bloque B",
                        "Un cuarto para bicicletas",
                        "Una lavandería compartida",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Cooperativa d'habitatge: prioritats per al 2027",
                    description: "Cada soci té 100 crèdits per a les cinc prioritats de la \
                        cooperativa el 2027 i n'ha de gastar almenys 50. Donar n vots a una \
                        prioritat costa n² crèdits, de manera que una preferència forta costa \
                        més que un suport repartit. La llista de socis és registrada a la \
                        cadena: els socis nous poden votar tan bon punt s'incorporen.",
                    question: "Quants vots dones a cada prioritat?",
                    question_description: "Gasta entre 50 i 100 crèdits; n vots costen n² \
                        crèdits.",
                    choices: &[
                        "Aïllar les façanes",
                        "Plaques solars als terrats",
                        "Un ascensor al bloc B",
                        "Un espai per a bicicletes",
                        "Una bugaderia compartida",
                    ],
                },
            ],
            lean: &[0.32, 0.27, 0.18, 0.10, 0.13],
            ballot: BallotKind::QuadraticBudget {
                budget: 100,
                min_spend: 50,
            },
            census: CensusKind::Contract { added: 6 },
            members: 12,
            key: KeySource::Node(0),
            lifecycle: Lifecycle::Tally,
            // Full after the first round, then raised for the new members.
            actions: &[Action::MaxVoters(30)],
            round1: 12,
            round2: (12, 18),
            revotes: 2,
            max_voters: 12,
            ..W2
        },
        Spec {
            n: 8,
            dir: "wave2/8-name-the-square",
            title: "Name the new square",
            description: "The new square between the market and the station needs a name. Rank \
                the five proposals from 1 (your favourite) to 5. The rankings stay sealed until \
                the organizers publish their part of the key after the vote closes.",
            question: "How do you rank the proposed names?",
            question_description: "Give each name a different rank, from 1 to 5.",
            choices: &[
                "Weavers' Square",
                "Clara Campoamor Square",
                "Old Mill Square",
                "Harbour Lights Square",
                "Linden Tree Square",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Pon nombre a la nueva plaza",
                    description: "La nueva plaza entre el mercado y la estación necesita un \
                        nombre. Ordena las cinco propuestas de 1 (tu favorita) a 5. Las \
                        clasificaciones quedan selladas hasta que los organizadores publiquen su \
                        parte de la clave, después del cierre.",
                    question: "¿Cómo ordenas los nombres propuestos?",
                    question_description: "Da a cada nombre una posición distinta, de 1 a 5.",
                    choices: &[
                        "Plaza de los Tejedores",
                        "Plaza de Clara Campoamor",
                        "Plaza del Molino Viejo",
                        "Plaza de las Luces del Puerto",
                        "Plaza del Tilo",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Posa nom a la plaça nova",
                    description: "La plaça nova entre el mercat i l'estació necessita un nom. \
                        Ordena les cinc propostes d'1 (la teva preferida) a 5. Les \
                        classificacions queden segellades fins que els organitzadors publiquin \
                        la seva part de la clau, després del tancament.",
                    question: "Com ordenes els noms proposats?",
                    question_description: "Dona a cada nom una posició diferent, d'1 a 5.",
                    choices: &[
                        "Plaça dels Teixidors",
                        "Plaça de Clara Campoamor",
                        "Plaça del Molí Vell",
                        "Plaça dels Llums del Port",
                        "Plaça del Til·ler",
                    ],
                },
            ],
            lean: &[0.26, 0.30, 0.20, 0.10, 0.14],
            ballot: BallotKind::Ranking,
            members: 18,
            key: KeySource::DkgLocked,
            lifecycle: Lifecycle::Tally,
            round1: 10,
            round2: (10, 15),
            revotes: 3,
            same_node: 1,
            // Two voters change their ranking twice.
            round3: (15, 16),
            revotes3: 2,
            max_voters: 18,
            ..W2
        },
        Spec {
            n: 9,
            dir: "wave2/9-water-tariff",
            title: "Water cooperative: tariff reform",
            description: "Resolution 2027-02: adopt the two-tier water tariff proposed by the \
                board. Each member votes with the weight of their shares, as certified by the \
                cooperative's census service, and puts that whole weight on one option.",
            question: "Do you approve the new tariff?",
            question_description: "Your whole weight goes to one option.",
            choices: &["In favour", "Against", "Abstain"],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Cooperativa de aguas: reforma de la tarifa",
                    description: "Resolución 2027-02: adoptar la tarifa de agua de dos tramos que \
                        propone el consejo rector. Cada socio vota con el peso de sus \
                        participaciones, certificado por el servicio de censo de la cooperativa, \
                        y pone todo ese peso en una sola opción.",
                    question: "¿Apruebas la nueva tarifa?",
                    question_description: "Todo tu peso va a una sola opción.",
                    choices: &["A favor", "En contra", "Abstención"],
                },
                Lang {
                    code: "ca",
                    title: "Cooperativa d'aigües: reforma de la tarifa",
                    description: "Resolució 2027-02: adoptar la tarifa d'aigua de dos trams que \
                        proposa el consell rector. Cada soci vota amb el pes de les seves \
                        participacions, certificat pel servei de cens de la cooperativa, i posa \
                        tot aquest pes en una sola opció.",
                    question: "Aproves la nova tarifa?",
                    question_description: "Tot el teu pes va a una sola opció.",
                    choices: &["A favor", "En contra", "Abstenció"],
                },
            ],
            lean: &[0.52, 0.33, 0.15],
            ballot: BallotKind::Weighted,
            census: CensusKind::Csp,
            members: 12,
            weights: (5, 400),
            key: KeySource::DkgAutomatic,
            lifecycle: Lifecycle::Tally,
            round1: 7,
            round2: (7, 11),
            revotes: 1,
            max_voters: 12,
            ..W2
        },
        Spec {
            n: 10,
            dir: "wave2/10-irrigation-works",
            title: "Irrigation community: works programme",
            description: "Members vote on next year's works with as many credits as the census \
                gives them, one per tenth of a hectare they irrigate. Giving a work v votes costs \
                v³ credits, which strongly rewards spreading support. The census is updated when \
                plots change hands, and new members can vote from then on.",
            question: "How many votes do you give each work?",
            question_description: "v votes cost v³ credits, up to 5 votes per work, within your \
                credits.",
            choices: &[
                "Line the main channel",
                "Replace the north sluice gates",
                "Automatic water meters",
                "Dredge the reservoir",
                "A solar pump for the well",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Comunidad de regantes: programa de obras",
                    description: "Los comuneros votan las obras del año que viene con tantos \
                        créditos como les asigna el censo, uno por cada décima de hectárea que \
                        riegan. Dar v votos a una obra cuesta v³ créditos, lo que premia mucho \
                        repartir el apoyo. El censo se actualiza cuando las parcelas cambian de \
                        manos, y los nuevos comuneros pueden votar desde ese momento.",
                    question: "¿Cuántos votos das a cada obra?",
                    question_description: "v votos cuestan v³ créditos, hasta 5 votos por obra, \
                        dentro de tus créditos.",
                    choices: &[
                        "Revestir la acequia principal",
                        "Cambiar las compuertas del norte",
                        "Contadores de agua automáticos",
                        "Dragar el embalse",
                        "Una bomba solar para el pozo",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Comunitat de regants: programa d'obres",
                    description: "Els comuners voten les obres de l'any vinent amb tants crèdits \
                        com els assigna el cens, un per cada dècima d'hectàrea que reguen. Donar \
                        v vots a una obra costa v³ crèdits, cosa que premia molt repartir el \
                        suport. El cens s'actualitza quan les parcel·les canvien de mans, i els \
                        comuners nous poden votar des d'aquell moment.",
                    question: "Quants vots dones a cada obra?",
                    question_description: "v vots costen v³ crèdits, fins a 5 vots per obra, \
                        dins dels teus crèdits.",
                    choices: &[
                        "Revestir la sèquia principal",
                        "Canviar les comportes del nord",
                        "Comptadors d'aigua automàtics",
                        "Dragar l'embassament",
                        "Una bomba solar per al pou",
                    ],
                },
            ],
            lean: &[0.30, 0.22, 0.18, 0.12, 0.18],
            ballot: BallotKind::CostExponent { exp: 3, cap: 5 },
            // Member 10 votes, then the update reweights them before the
            // vote settles: the node errors it and they vote again.
            census: CensusKind::Updatable {
                added: 5,
                reweight: Some(10),
            },
            members: 16,
            weights: (20, 150),
            key: KeySource::Node(1),
            lifecycle: Lifecycle::Tally,
            round1: 10,
            round2: (10, 21),
            revotes: 2,
            max_voters: 30,
            ..W2
        },
        Spec {
            n: 11,
            dir: "wave2/11-membership-fee",
            title: "What should next year's membership fee be?",
            description: "The association is setting next year's annual membership fee. Enter \
                the amount in euros, from 0 to 100, that you think is fair. The fee will be the \
                average of all answers, rounded to the nearest euro.",
            question: "How much should the annual fee be, in euros?",
            question_description: "A whole number from 0 to 100.",
            choices: &["Annual fee in euros"],
            i18n: &[
                Lang {
                    code: "es",
                    title: "¿Cuál debería ser la cuota del año que viene?",
                    description: "La asociación fija la cuota anual del año que viene. Escribe \
                        la cantidad en euros, de 0 a 100, que te parezca justa. La cuota será la \
                        media de todas las respuestas, redondeada al euro.",
                    question: "¿Cuánto debería ser la cuota anual, en euros?",
                    question_description: "Un número entero de 0 a 100.",
                    choices: &["Cuota anual en euros"],
                },
                Lang {
                    code: "ca",
                    title: "Quina hauria de ser la quota de l'any vinent?",
                    description: "L'associació fixa la quota anual de l'any vinent. Escriu la \
                        quantitat en euros, de 0 a 100, que et sembli justa. La quota serà la \
                        mitjana de totes les respostes, arrodonida a l'euro.",
                    question: "Quant hauria de ser la quota anual, en euros?",
                    question_description: "Un nombre enter de 0 a 100.",
                    choices: &["Quota anual en euros"],
                },
            ],
            lean: &[0.42],
            ballot: BallotKind::Numeric { max: 100 },
            members: 20,
            key: KeySource::DkgAutomatic,
            lifecycle: Lifecycle::Tally,
            metadata: MetaPlan::WhileOpen(&FEE_CLARIFIED),
            round1: 12,
            round2: (12, 17),
            revotes: 2,
            max_voters: 20,
            ..W2
        },
        Spec {
            n: 12,
            dir: "wave2/12-congress-venue",
            title: "Board decision: venue of the spring congress",
            description: "The board has eight voting seats and twelve members and substitutes \
                on its list. The first eight ballots fill the seats; once they are taken, \
                further ballots are refused, although a seated member can still change their \
                vote.",
            question: "Where should the spring congress be held?",
            question_description: "Pick one venue.",
            choices: &[
                "The cultural centre",
                "The sports pavilion",
                "The town theatre",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Decisión de la junta: sede del congreso de primavera",
                    description: "La junta tiene ocho plazas con voto y doce titulares y \
                        suplentes en su lista. Las ocho primeras papeletas ocupan las plazas; \
                        una vez ocupadas, las siguientes se rechazan, aunque quien ya tiene plaza \
                        puede cambiar su voto.",
                    question: "¿Dónde debería celebrarse el congreso de primavera?",
                    question_description: "Elige una sede.",
                    choices: &[
                        "El centro cultural",
                        "El pabellón deportivo",
                        "El teatro municipal",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Decisió de la junta: seu del congrés de primavera",
                    description: "La junta té vuit places amb vot i dotze titulars i suplents a \
                        la llista. Les vuit primeres paperetes ocupen les places; un cop \
                        ocupades, les següents es rebutgen, tot i que qui ja té plaça pot canviar \
                        el seu vot.",
                    question: "On s'hauria de fer el congrés de primavera?",
                    question_description: "Tria una seu.",
                    choices: &[
                        "El centre cultural",
                        "El pavelló esportiu",
                        "El teatre municipal",
                    ],
                },
            ],
            lean: &[0.45, 0.20, 0.35],
            ballot: BallotKind::SingleChoice { abstain: false },
            members: 12,
            key: KeySource::Node(0),
            lifecycle: Lifecycle::Tally,
            actions: &[Action::Refuse(Refusal::OverMaxVoters)],
            round1: 8,
            round2: (8, 8),
            revotes: 2,
            same_node: 1,
            max_voters: 8,
            ..W2
        },
        Spec {
            n: 13,
            dir: "wave2/13-dog-park",
            title: "Where should the new dog park go?",
            description: "The parks department has three possible sites for a fenced dog park. \
                Pick the one you prefer; the department will build on the site with the most \
                votes.",
            question: "Which site do you prefer?",
            question_description: "Pick one site.",
            choices: &[
                "The north corner of the central park",
                "Next to the sports ground",
                "The empty lot on Mill Street",
            ],
            lean: &[0.40, 0.25, 0.35],
            ballot: BallotKind::SingleChoice { abstain: false },
            members: 16,
            key: KeySource::Node(1),
            lifecycle: Lifecycle::Canceled,
            round1: 10,
            round2: (10, 10),
            max_voters: 16,
            ..W2
        },
        Spec {
            n: 14,
            dir: "wave2/14-photography-themes",
            title: "Photography competition: choose the themes",
            description: "Choose one or two themes for this year's photography competition. The \
                two with the most votes will be announced at the opening of the exhibition.",
            question: "Which themes would you like?",
            question_description: "Choose one or two.",
            choices: &["Water", "Hands at work", "The city at night", "Neighbours"],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Concurso de fotografía: elige los temas",
                    description: "Elige uno o dos temas para el concurso de fotografía de este \
                        año. Los dos más votados se anunciarán en la inauguración de la \
                        exposición.",
                    question: "¿Qué temas te gustarían?",
                    question_description: "Elige uno o dos.",
                    choices: &[
                        "El agua",
                        "Manos que trabajan",
                        "La ciudad de noche",
                        "Vecinos",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Concurs de fotografia: tria els temes",
                    description: "Tria un o dos temes per al concurs de fotografia d'aquest any. \
                        Els dos més votats s'anunciaran a la inauguració de l'exposició.",
                    question: "Quins temes t'agradarien?",
                    question_description: "Tria'n un o dos.",
                    choices: &["L'aigua", "Mans que treballen", "La ciutat de nit", "Veïns"],
                },
            ],
            lean: &[0.30, 0.25, 0.25, 0.20],
            ballot: BallotKind::MultipleChoice { min: 1, max: 2 },
            members: 8,
            key: KeySource::Node(0),
            lifecycle: Lifecycle::CanceledEarly { start_in: DAY },
            max_voters: 8,
            ..W2
        },
        Spec {
            n: 15,
            dir: "wave2/15-community-kitchen",
            title: "Community kitchen: the Saturday lunch menu",
            description: "The community kitchen serves a shared lunch every Saturday. Choose the \
                menu we cook most often next season; the others will rotate.",
            question: "Which menu should we cook most often?",
            question_description: "Pick one menu.",
            choices: &[
                "Paella and salad",
                "Lentil stew",
                "Couscous with vegetables",
                "Pasta with seasonal pesto",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Cocina comunitaria: el menú de los sábados",
                    description: "La cocina comunitaria sirve una comida compartida cada sábado. \
                        Elige el menú que cocinaremos más a menudo la próxima temporada; los \
                        demás irán rotando.",
                    question: "¿Qué menú deberíamos cocinar más a menudo?",
                    question_description: "Elige un menú.",
                    choices: &[
                        "Paella y ensalada",
                        "Lentejas estofadas",
                        "Cuscús con verduras",
                        "Pasta con pesto de temporada",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Cuina comunitària: el menú dels dissabtes",
                    description: "La cuina comunitària serveix un dinar compartit cada dissabte. \
                        Tria el menú que cuinarem més sovint la temporada vinent; els altres \
                        aniran rotant.",
                    question: "Quin menú hauríem de cuinar més sovint?",
                    question_description: "Tria un menú.",
                    choices: &[
                        "Paella i amanida",
                        "Llenties estofades",
                        "Cuscús amb verdures",
                        "Pasta amb pesto de temporada",
                    ],
                },
            ],
            lean: &[0.34, 0.20, 0.26, 0.20],
            ballot: BallotKind::SingleChoice { abstain: false },
            members: 16,
            key: KeySource::Node(1),
            lifecycle: Lifecycle::Later {
                start_in: 30 * MINUTE,
            },
            metadata: MetaPlan::BeforeStart(&LUNCHES_CORRECTED),
            round3: (0, 12),
            max_voters: 16,
            ..W2
        },
        Spec {
            n: 16,
            dir: "wave2/16-library-hours",
            title: "Library: extra opening hours during exams",
            description: "During the June exams the library can open for extra hours. Choose \
                when they should be; voting opens a few days after this announcement.",
            question: "When should the library open longer?",
            question_description: "Pick one option.",
            choices: &[
                "Early mornings, from 7:00",
                "Late evenings, until midnight",
                "Sunday afternoons",
            ],
            lean: &[0.30, 0.45, 0.25],
            ballot: BallotKind::SingleChoice { abstain: false },
            members: 10,
            key: KeySource::Node(0),
            lifecycle: Lifecycle::Upcoming {
                start_in: 3 * DAY,
                secs: 2 * DAY,
            },
            max_voters: 10,
            ..W2
        },
        Spec {
            n: 17,
            dir: "wave2/17-watering-schedule",
            title: "Allotment gardens: new watering schedule",
            description: "This election is a demonstration of a metadata mismatch: the hash \
                registered on-chain is that of an earlier draft of this document, so explorers \
                must show its text as unverified. The ballots it receives are counted as usual. \
                The question itself is real: the allotment gardens need a new watering schedule \
                for the summer.",
            question: "Which watering slot do you prefer?",
            question_description: "Pick one slot.",
            choices: &[
                "Early morning, 6:00 to 9:00",
                "Evening, 19:00 to 22:00",
                "Alternate days, all day",
            ],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Huertos urbanos: nuevo horario de riego",
                    description: "Esta votación es una demostración de un desajuste de \
                        metadatos: el hash registrado en la cadena es el de un borrador anterior \
                        de este documento, así que los exploradores deben mostrar su texto como \
                        no verificado. Las papeletas que reciba se cuentan con normalidad. La \
                        pregunta es real: los huertos urbanos necesitan un nuevo horario de riego \
                        para el verano.",
                    question: "¿Qué franja de riego prefieres?",
                    question_description: "Elige una franja.",
                    choices: &[
                        "Primera hora, de 6:00 a 9:00",
                        "Tarde, de 19:00 a 22:00",
                        "Días alternos, todo el día",
                    ],
                },
                Lang {
                    code: "ca",
                    title: "Horts urbans: nou horari de reg",
                    description: "Aquesta votació és una demostració d'un desajust de metadades: \
                        el hash registrat a la cadena és el d'un esborrany anterior d'aquest \
                        document, de manera que els exploradors n'han de mostrar el text com a no \
                        verificat. Les paperetes que rebi es compten amb normalitat. La pregunta \
                        és real: els horts urbans necessiten un nou horari de reg per a l'estiu.",
                    question: "Quina franja de reg prefereixes?",
                    question_description: "Tria una franja.",
                    choices: &[
                        "A primera hora, de 6:00 a 9:00",
                        "Al vespre, de 19:00 a 22:00",
                        "Dies alterns, tot el dia",
                    ],
                },
            ],
            lean: &[0.45, 0.40, 0.15],
            ballot: BallotKind::SingleChoice { abstain: false },
            members: 12,
            key: KeySource::Node(0),
            lifecycle: Lifecycle::Open { secs: 5 * DAY },
            metadata: MetaPlan::Mismatch(&WATERING_DRAFT),
            round1: 6,
            round2: (6, 6),
            max_voters: 12,
            ..W2
        },
        Spec {
            n: 18,
            dir: "wave2/18-youth-section-name",
            title: "Rename the youth section?",
            description: "The board proposes renaming the youth section to Young Members. Vote \
                yes or no.",
            question: "Should the youth section be renamed?",
            question_description: "Pick one option.",
            choices: &["Yes", "No"],
            lean: &[0.5, 0.5],
            ballot: BallotKind::SingleChoice { abstain: false },
            members: 10,
            key: KeySource::Node(1),
            lifecycle: Lifecycle::Tally,
            max_voters: 10,
            ..W2
        },
        Spec {
            n: 19,
            dir: "wave2/19-new-statutes",
            title: "Extraordinary assembly: new statutes",
            description: "The extraordinary assembly votes on the new statutes drafted by the \
                working group. The ballots are encrypted with a key held jointly by an \
                independent committee.",
            question: "Do you approve the new statutes?",
            question_description: "Pick one option.",
            choices: &["Approve", "Reject", "Abstain"],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Asamblea extraordinaria: nuevos estatutos",
                    description: "La asamblea extraordinaria vota los nuevos estatutos que ha \
                        redactado el grupo de trabajo. Las papeletas se cifran con una clave que \
                        custodia de forma conjunta un comité independiente.",
                    question: "¿Apruebas los nuevos estatutos?",
                    question_description: "Elige una opción.",
                    choices: &["Aprobar", "Rechazar", "Abstenerse"],
                },
                Lang {
                    code: "ca",
                    title: "Assemblea extraordinària: nous estatuts",
                    description: "L'assemblea extraordinària vota els nous estatuts que ha \
                        redactat el grup de treball. Les paperetes es xifren amb una clau que \
                        custodia de manera conjunta un comitè independent.",
                    question: "Aproves els nous estatuts?",
                    question_description: "Tria una opció.",
                    choices: &["Aprovar", "Rebutjar", "Abstenir-se"],
                },
            ],
            lean: &[0.5, 0.3, 0.2],
            ballot: BallotKind::SingleChoice { abstain: false },
            members: 10,
            key: KeySource::DkgAutomatic,
            lifecycle: Lifecycle::Tally,
            max_voters: 10,
            ..W2
        },
    ]
}
